# Running an X server (`startx`) on Eclipse OS

[Español](README-xorg.md) · **English**

Eclipse OS (zCore) exposes the console/virtual-terminal layer a Linux X server
needs to take control of the screen. This document explains what the kernel
supports and how userspace is configured so that `startx` works.

## What the kernel provides

The kernel implements the devices and `ioctl`s that `Xorg` uses in its
`xf86OpenConsole` routine:

- **Console nodes**: `/dev/tty`, `/dev/tty0` (active VT), `/dev/console` and
  `/dev/tty1`..`/dev/tty6`.
- **VT `ioctl`s** (`<linux/vt.h>`): `VT_OPENQRY`, `VT_GETMODE`, `VT_SETMODE`,
  `VT_GETSTATE`, `VT_ACTIVATE`, `VT_WAITACTIVE`, `VT_RELDISP`, `VT_DISALLOCATE`.
- **KD `ioctl`s** (`<linux/kd.h>`): `KDGETMODE`/`KDSETMODE`
  (`KD_TEXT`/`KD_GRAPHICS`, per VT), `KDGKBMODE`/`KDSKBMODE` (keyboard mode) and
  `KDGKBTYPE`. When X puts the keyboard in `K_OFF`/`K_RAW`, the kernel stops
  injecting "cooked" characters into that TTY; raw events still arrive through
  `/dev/input/event*`.
- **Framebuffer**: `/dev/fb0` with `FBIOGET_VSCREENINFO`/`FBIOGET_FSCREENINFO`
  and, since the resolution is fixed, it accepts `FBIOPUT_VSCREENINFO`,
  `FBIOPAN_DISPLAY` and `FBIOBLANK` so that X's `fbdev` driver starts.
- **Input**: `/dev/input/event*` and `/dev/input/mice` (PS/2, USB HID and
  virtio).
- **DRM**: `/dev/dri/card0` (partial).
- **VT switching**: Ctrl+Alt+F1..F6. The KD mode is per VT, so leaving X's
  graphical VT still shows a normal text console.

## Userspace configuration

`cargo xtask image` installs the packages and generates the configuration; none
of this has to be done by hand on an image built with xtask
(`xtask/src/linux/desktop.rs`, `write_xorg_config`). To install it by hand with
Ubuntu packages:

```sh
apt-get install xserver-xorg-core xserver-xorg-video-fbdev xserver-xorg-input-libinput xinit libgl1-mesa-dri
```

The generated file is `/etc/X11/xorg.conf.d/10-eclipse.conf`:

```
Section "ServerFlags"
    Option "AutoAddDevices" "true"    # libinput classifies each /dev/input/event*
    Option "DontZap"        "false"
    # Do not touch DRM: modern Xorg probes /dev/dri/card0 as a GPU even when
    # the Device is fbdev, and on this kernel that probe hangs ("Platform
    # probe for /sys/class/drm/card0" in Xorg.0.log; startx times out and init
    # relaunches X every ~90 s). Without GPU auto-add/auto-bind, X stays on
    # the fbdev path.
    Option "AutoAddGPU"     "false"
    Option "AutoBindGPU"    "false"
EndSection

Section "Device"
    Identifier "fb"
    Driver     "fbdev"
    Option     "fbdev"    "/dev/fb0"
    Option     "ShadowFB" "true"
EndSection

Section "Screen"
    Identifier "screen"
    Device     "fb"
EndSection

# libinput for every enumerated evdev node. No MatchIsKeyboard/MatchIsPointer:
# they depend on udev's ID_INPUT_* tags, which this kernel does not emit, so
# they would match nothing. libinput classifies the device by its own
# capabilities.
Section "InputClass"
    Identifier      "eclipse-libinput"
    MatchDevicePath "/dev/input/event*"
    Driver          "libinput"
EndSection

Section "ServerLayout"
    Identifier  "layout"
    Screen      "screen"
EndSection
```

`fbdev` on `/dev/fb0` is used instead of `modesetting`: scanout is software
anyway, so writing straight into the linear framebuffer saves the dumb buffer +
KMS commit per frame. The `/dev/fb0` mapping is write-combining.

## Trying it

```sh
startx
```

To see the server log for diagnosis:

```sh
Xorg -verbose 6 :0 vt1 2> /tmp/Xorg.log ; cat /tmp/Xorg.log
```

## Diagnosis: `startx` does not start and *no X log appears*

If `startx` ends instantly and neither `/tmp/Xorg.log` nor
`/var/log/Xorg.0.log` is created, the X server most likely **dies before
reaching `main()`**, inside the dynamic loader (a shared library or a symbol is
missing). `Xorg` links against many more `.so` files than a console
application, so a single missing one is enough for it to abort without writing
anything to its own log.

The kernel now logs the `exec` chain and dynamic loader errors to `dmesg`. After
trying `startx`, look at the kernel log:

```sh
dmesg | grep -E 'EXECVE|XLOG'
```

- `EXECVE[pid] "/path" argv=[...]` lines show exactly which binaries run. If
  **no** `EXECVE` with `Xorg`/`X`/`Xorg.wrap` appears, the problem is in
  `xinit`/`startx` (it cannot find the server): check `~/.xinitrc`, `$PATH` and
  that `/usr/bin/X` points to the server.
- `XLOG: Error loading shared library ...` or
  `XLOG: Error relocating ...: symbol not found` lines name the missing library
  or symbol. Install the package that provides it.

You can also run the server by hand to see the linker error right away (it is
written to `stderr`):

```sh
Xorg -version            # if it prints a version, dynamic linking is fine
LD_TRACE_LOADED_OBJECTS=1 Xorg   # lists the .so files it needs and which are missing
```

If `Xorg -version` fails with `Error loading shared library libfoo.so.N`,
install the corresponding package (`apt-get install ...`) and retry. Once
`Xorg -version` prints the version, linking is no longer the problem and you
should look at `/tmp/Xorg.log` (previous section) for the next failure.

## Notes and limitations

- `VT_OPENQRY` returns the active VT, so X takes over the terminal `startx` was
  launched from (and goes back to it on exit).
- Handover via `VT_PROCESS` signals (relsig/acqsig) is implemented Linux-style:
  on Ctrl+Alt+Fn the kernel sends `relsig` to the owner of the graphical VT and
  waits for its `VT_RELDISP` before switching; on the way back it sends
  `acqsig` so the server regains DRM master and repaints. If the compositor does
  not cooperate (VT_AUTO, or it hangs), the switch is immediate or is forced by
  pressing Ctrl+Alt+Fn again, and the suppression of blits from the inactive
  graphical VT (see `drm.rs`) keeps the text console visible as a safety net.
- The X session uses `fbdev` and Mesa renders in software (`llvmpipe`, the
  `libgl1-mesa-dri` package). GPU acceleration exists in the Wayland session
  with NVIDIA and `nvidia.nouveau_uapi` (see
  [README-nouveau-uapi.en.md](README-nouveau-uapi.en.md)), not in X.

## Pseudo-terminals (PTY)

Terminal emulators under X (xterm, st, rxvt…) need pseudo-terminals to start a
shell. Eclipse OS exposes them the same way Linux does:

- `/dev/ptmx`: opening this node creates a new master/slave pair and returns the
  *master*. `ptsname(3)` resolves the number with `ioctl(TIOCGPTN)` and
  `unlockpt(3)` uses `ioctl(TIOCSPTLCK)` (accepted; not required to open the
  slave).
- `/dev/pts/N`: the *slave*. The program running on it (the shell) sees it as a
  real terminal: `tcgetattr`/`tcsetattr`, `TIOCGWINSZ`/`TIOCSWINSZ`, foreground
  process groups (`TIOCGPGRP`/`TIOCSPGRP`) and signals (`Ctrl+C`→SIGINT,
  `SIGWINCH` on resize, `SIGHUP` when the master closes).

The line discipline (canonical/raw mode, echo, CR/NL translation) runs on the
slave side: what the emulator writes to the master is processed and, with echo
on, comes back to the master so the terminal shows what was typed.
