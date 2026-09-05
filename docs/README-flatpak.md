# Flatpak en Eclipse OS

Eclipse sigue el modelo de [EU OS](https://eu-os.eu/): el **sistema** se construye e instala con **apk** (repos propios cuando los tengáis); las **aplicaciones de usuario** van por **Flatpak**.

## Qué hay en la imagen

Paquetes (apk, community Alpine): `flatpak`, `ostree`, `bubblewrap`, `xdg-dbus-proxy`, `xdg-desktop-portal`, `xdg-desktop-portal-gtk`, `xdg-desktop-portal-wlr`, `fuse`/`fuse3`.

Servicios de init:

| Servicio | Función |
|----------|---------|
| `dbus-session` | `dbus-daemon` en `unix:path=/run/user/0/bus` (la dirección que ya pincha el entorno) |
| `xdg-desktop-portal` | Portals (archivos, captura, …) tras labwc |
| `flatpak-setup` | `flatpak remote-add --if-not-exists flathub` cuando hay red |

Repo de sistema: `/var/lib/flatpak` (tmpfs en live squashfs). Remote: Flathub.

## Kernel (sandbox)

Bubblewrap deja de recibir `ENOSYS`/`EINVAL` en `unshare`/`clone(CLONE_NEW*)`. Hay:

- user ns + `/proc/<pid>/uid_map` / `gid_map` / `setgroups`
- mount ns con overlay (bind de directorios, tmpfs real, `MS_PRIVATE`)
- `chroot` / `pivot_root`
- net ns (sin `AF_INET`/`AF_INET6`; UNIX sigue para dbus/Wayland/Pulse)
- seccomp BPF clásico (`seccomp(2)` y `prctl(PR_SET_SECCOMP)`)

`/dev/fuse` todavía no está: el **document portal** (abrir archivos del host) degradará hasta que FUSE entre. El resto de `flatpak` (`install`, `update`, `uninstall`, `list`, `info`, `run`, `override`, `remotes`, `search`, `repair`, `config`, `enter`, `ps`, `mask`, `pin`, `history`, `make-current`, …) usa ostree + bwrap + D-Bus.

## Uso

```sh
flatpak remotes
flatpak search firefox
flatpak install --system flathub org.mozilla.firefox
flatpak run org.mozilla.firefox
flatpak list
flatpak override --user --filesystem=home org.mozilla.firefox
```

Logs: `/tmp/dbus-session.log`, `/tmp/xdg-desktop-portal.log`, `/tmp/flatpak-setup.log`.

Probar el sandbox sin Flathub:

```sh
bwrap --unshare-all --ro-bind /usr /usr --ro-bind /lib /lib --dev /dev --chdir / /usr/bin/true
echo $?
```
