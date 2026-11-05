# D-Bus on Eclipse OS

[Español](README-dbus.md) · **English**

Eclipse starts a real **session bus**. Until recently there was none: the
address `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus` was deliberately
pinned to a socket **with no daemon behind it**, so that `connect()` failed
instantly with `ECONNREFUSED` and libdbus would not try `autolaunch:` (which
forks `dbus-launch`, opens `$DISPLAY` and hangs whoever waits for it: that is
how gzdoom hung). That avoided the hang, but left everything that needs a bus
without one.

Now a daemon listens on that same path, so no environment variable had to
change: the four places that already exported the address (`eclipse-init`,
`/etc/profile`, `~/.config/labwc/environment` and the SDL wrapper) stay the same
and clients find the bus on their own.

## What starts, and in what order

| Piece | Where |
|---|---|
| Service | `/etc/eclipse/services/dbus.service` (`type = respawn`) |
| Wrapper | `/usr/local/bin/eclipse-dbus` |
| Daemon (`dbus` package) | `dbus-daemon --session --nofork --nopidfile --address=unix:path=/run/user/0/bus` |
| Daemon (Eclipse) | `/bin/eclipse-dbusd --session --address=unix:path=/run/user/0/bus` |

The wrapper prefers `dbus-daemon` from the `dbus` package when it is installed
(on the desktop variant it comes from Ubuntu, the `DESKTOP_UBUNTU` list in
`xtask/src/linux/apt_rootfs.rs`; with `ECLIPSE_PKG=apk`, from Alpine) and falls
back to `eclipse-dbusd` otherwise. Both accept the same `argv`, so the choice is
a `command -v`.

The service does **not** carry `desktop =`: both the labwc and the Xorg session
want it. `labwc.service` and `xorg.service` carry `after = dbus`, but **without**
`wait_socket`: a missing bus must cost the boot nothing, and no boot service
talks to it — the clients that do (SDL games, GTK applications) are launched
minutes later.

The wrapper also takes care of `/etc/machine-id` and `/var/lib/dbus/machine-id`
(dbus checks they are 32 lowercase hex digits and refuses to start with anything
else) and of deleting a socket orphaned by a previous boot, which would make
`bind()` fail with `EADDRINUSE` with nobody listening.

## `eclipse-dbusd`: our own bus

`tools/eclipse-dbusd` is a session bus written in Rust, statically linked
against musl, with no dependency other than `libc`. It exists for two concrete
reasons:

1. An image built **without access to packages** (the `minimal` variant, or an
   offline build) has no `dbus-daemon`, and without a bus nothing that uses
   `GtkApplication` starts, nor anything that registers a name to check whether
   another copy is already running.
2. It is our own code, so it can be **tested against the Eclipse kernel in
   QEMU** instead of only on hardware. Every step of the probe is a round trip
   through `sendmsg`/`recvmsg`/`poll` on a unix socket, and EXTERNAL
   authentication relies on `SO_PEERCRED`.

### What it implements

- The `unix:path=` transport, the SASL handshake (`EXTERNAL` with
  `SO_PEERCRED`, and `ANONYMOUS`) and `NEGOTIATE_UNIX_FD`.
- Unique names (`:1.N`), `Hello`, and the whole well-known name machinery:
  `RequestName`/`ReleaseName` with a replacement queue,
  `NameOwnerChanged`/`NameAcquired`/`NameLost`.
- Unicast routing by destination with the `SENDER` field set by the bus (a
  client cannot forge it), error replies for calls that cannot be delivered,
  signal broadcast filtered by `AddMatch` rules, and descriptor passing
  (`SCM_RIGHTS`).
- `BecomeMonitor`, so `dbus-monitor` works: on a machine without a debugger it
  is the only way to see what is being said on the session.

### What it does NOT implement, on purpose

- **Service activation.** It does not read `.service` files or launch anything
  on demand; `StartServiceByName` answers `ServiceUnknown` unless the name is
  already on the bus. On Eclipse no daemon is activated on demand: they are all
  `eclipse-init` services.
- **Policy (`<allow>`/`<deny>` from `session.conf`).** Eclipse is single-user
  root: the bus rejects connections whose `SO_PEERCRED` uid is not its own and,
  beyond that, everything is allowed.

If activation or policy is ever needed, the answer is to install the `dbus`
package: the wrapper prefers it on its own.

## Checking that it works

From a terminal in the session, or from the desktop menu ("D-Bus test (session
bus)"):

```sh
eclipse-dbusd --selftest          # DBUSPROBE: PASS ... / DBUSPROBE: FAIL ...
```

The probe opens two connections and walks through what a desktop client really
uses: distinct unique names, `AddMatch`, `RequestName`, `NameOwnerChanged`,
`GetNameOwner`, a call routed by well-known name with its reply coming back, a
broadcast signal, `Ping`, `ListNames`, and the name being released on
disconnect. It exits with code 1 and a `FAIL` line saying which step failed.

In QEMU, where there is no desktop, it can be run at boot:

```sh
make -C zCore ... CMDLINE="...:dbus.selftest"
```

`dbus-selftest.service` carries `cmdline = dbus.selftest`, an `eclipse-init`
key that starts a service only when that token is on the kernel command line.
On a normal boot the service is not even loaded.

With the `dbus` package installed, the usual tools work the same:

```sh
dbus-send --print-reply --dest=org.freedesktop.DBus / org.freedesktop.DBus.ListNames
gdbus call --session --dest org.freedesktop.DBus \
  --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.GetId
busctl --user list
dbus-monitor --session
```

All four have been tested against `eclipse-dbusd` (libdbus, GDBus, sd-bus and
the monitor), not only against `dbus-daemon`.

## What this unblocks

- **SDL2.** `SDL_Init()` runs `SDL_DBus_Init()` before anything else. It no
  longer hung (the pinned address prevented that), but now it also has a bus,
  which is what it uses to inhibit the screensaver.
- **GTK.** `GtkApplication` exits before drawing without a session bus: that is
  what killed waybar back in the day.
- **Single instance.** Everything that asks "is there another copy of me
  already?" does so with `RequestName`.

What **still cannot run** is Plasma: `plasmashell`, `kded`, `krunner` and the
polkit agent are D-Bus services, yes, but they also need Qt, KF6, polkit and
logind, which are not there. See [README-desktop.md](README-desktop.md).

## Ruled out: the bus was NOT gzdoom's hang (28 Sep 2026)

For a few hours it was believed that gzdoom hung inside `SDL_DBus_Init()`,
because the last thing it prints is its banner and right after that it calls
`SDL_Init(0)`, and the first thing `SDL_InitSubSystem` does in SDL2
(`src/SDL.c`) is connect to the bus. It fits, but it is false: launching the
game with

```sh
DBUS_SESSION_BUS_ADDRESS=unix:path=/sin-bus freedoom2
```

— an address with nothing behind it, where `connect()` fails instantly — gzdoom
hangs exactly the same, and deaf to `^C` as well.

The real cause is before SDL, in gzdoom's own prologue: `seteuid()`. In musl
that is `__synccall`, which blocks all application signals and stops the other
threads one by one with `tkill`. The kernel was unable to deliver a signal to a
thread whose stack comes from `mmap(PROT_NONE)` + `mprotect(RW)`, which is how
musl creates all its thread stacks; the details are in the PR that fixes it and
in the comment on `PageFlags::first`.

`eclipse-freedoom` keeps the bus muzzle (`ECLIPSE_FREEDOOM_BUS=1` disables it)
because a game does not need the bus — SDL only uses it to inhibit the
screensaver and for ibus — and one dependency fewer at start-up is one source
of hangs fewer. But it is not the fix for anything, and removing it should not
change the behaviour.
