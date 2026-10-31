mod apt_rootfs;
mod btrfs_image;
mod cuda_probe;
mod desktop;
mod eclipsefs_image;
mod hunterd;
mod image;
mod locales;
mod logo;
mod nvctl_probe;
pub(crate) mod nvidia_firmware;
mod nvidia_userspace;
mod ollama_cuda;
mod opencv;
mod os_release;
mod portal;
mod signal_probe;
mod test;
mod tree_image;
mod usb_probe;
mod verify;
mod xorg;

use crate::{commands::fetch_online, variant::Variant, Arch, PROJECT_DIR, REPOS, TARGET};
use os_xtask_utils::{dir, CommandExt, Ext, Git, Make};
use std::{
    env,
    ffi::OsString,
    fs,
    os::unix,
    path::{Path, PathBuf},
};

pub(crate) struct LinuxRootfs(Arch, Variant);

/// What makes every userspace tool in the rootfs a static, non-PIE ET_EXEC,
/// like the busybox and apk beside them.
///
/// `+crt-static` is not redundant with the musl triple. `x86_64-unknown-linux-musl`
/// and `aarch64-unknown-linux-musl` carry `crt-static-default: true` in their
/// target spec; **`riscv64gc-unknown-linux-musl` does not**, so without this
/// flag the riscv64 tools came out dynamically linked while the comments above
/// them said static. It cost nothing while none of those binaries needed a
/// symbol the Eclipse rootfs does not export -- and then `panic = "unwind"` in
/// eclipse-init (#1748) had PID 1 importing `_Unwind_RaiseException`,
/// `_Unwind_Resume` and friends from the `libgcc_s.so` that rustc links
/// dynamically for the unwinder on that target. The rootfs has no such
/// library: "Error relocating /sbin/init: symbol not found", `init(1) exited
/// with code 127`, and riscv64 did not boot at all. Linked static, the
/// unwinder comes from the `libunwind.a` the Rust musl target already ships
/// beside its `libc.a` and nothing is imported.
const STATIC_RUSTFLAGS: &str = "-C relocation-model=static -C target-feature=+crt-static";

/// The five binaries `tools/lunarbar` builds, as paths in its target directory.
///
/// A struct and not a tuple: five paths of the same type in one return value
/// are five chances to copy the launcher over the file browser, and the day
/// another binary joins them the struct is the thing that will not compile
/// quietly in the wrong order.
struct LunarTools {
    /// The panel.
    bar: PathBuf,
    /// The launcher behind Alt+Space and Super+D.
    run: PathBuf,
    /// The file browser behind Super+E, and its `--pick` chooser.
    file: PathBuf,
    /// The clipboard daemon and its Super+V history picker.
    clip: PathBuf,
    /// hunter's window, opened from the bar's shield menu.
    hunter: PathBuf,
}

impl LinuxRootfs {
    /// 生成指定架构的 linux rootfs 操作对象。The `desktop` variant, i.e. the
    /// historical behaviour.
    #[inline]
    pub const fn new(arch: Arch) -> Self {
        Self(arch, Variant::Desktop)
    }

    /// Same, for an explicit variant (see [`Variant`]).
    #[inline]
    pub const fn with_variant(arch: Arch, variant: Variant) -> Self {
        Self(arch, variant)
    }

    /// Which variant is being built.
    #[inline]
    pub const fn variant(&self) -> Variant {
        self.1
    }

    /// Path of an intermediate artifact under `ignored/target`, with the
    /// variant's suffix inserted BEFORE the extension: `efi.img.gz` for
    /// `desktop`, `efi-minimal.img.gz` for `minimal`.
    ///
    /// Giving each variant its own files is what lets them be built one after
    /// the other without the second overwriting the first one's payloads —
    /// `make release` publishes both ISOs in one run, and the ISO target picks
    /// up the payloads of the variant it is packaging rather than whichever
    /// build finished last.
    pub fn artifact(&self, stem: &str, ext: &str) -> PathBuf {
        TARGET.join(format!("{stem}{}.{ext}", self.1.suffix()))
    }

    /// [`Self::artifact`] for a working directory, which carries no extension:
    /// `live-rootfs` / `live-rootfs-minimal`.
    pub fn artifact_dir(&self, stem: &str) -> PathBuf {
        TARGET.join(format!("{stem}{}", self.1.suffix()))
    }

    /// The live SFS image this variant boots: `zCore/x86_64.img` for desktop,
    /// `zCore/x86_64-minimal.img` for minimal.
    ///
    /// Every launcher must go through this. The path used to be spelled out
    /// wherever it was needed, and when the variant axis arrived the two sites
    /// in `image.rs` learned the suffix while the two in `build.rs` did not:
    /// `cargo qemu --variant minimal` built the minimal image and then booted
    /// the desktop one, silently, or booted nothing at all on a tree that had
    /// only ever built minimal.
    pub fn live_image(&self) -> PathBuf {
        PROJECT_DIR
            .join("zCore")
            .join(format!("{}{}.img", self.0.name(), self.1.suffix()))
    }

    /// 构造启动内存文件系统 rootfs。
    /// 对于 x86_64，这个文件系统可用于 libos 启动。
    /// 若设置 `clear`，将清除已存在的目录。
    pub fn make(&self, clear: bool) {
        // 若已存在且不需要清空，可以直接退出
        let dir = self.path();
        if dir.is_dir() && !clear {
            Self::install_ca_certs(&dir);
            let musl = self.0.linux_musl_cross();
            let bin = dir.join("bin");
            // Ensure busybox applet symlinks are present even on incremental builds.
            // Without this, a rootfs built before symlink support was added (or from a
            // partial build) would produce a rootfs image where `ls`, `cat`, etc. are
            // missing, making the installed system unusable.
            Self::ensure_busybox_applets(&bin);
            let nl_dump = self.nl_dump(&musl);
            if nl_dump.is_file() {
                let _ = fs::copy(&nl_dump, bin.join("nl_dump"));
            }
            let edhcpc = self.edhcpc(&musl);
            if edhcpc.is_file() {
                let _ = fs::copy(&edhcpc, bin.join("edhcpc"));
            }
            let install_eclipse = self.install_eclipse(&musl);
            if install_eclipse.is_file() {
                let _ = fs::copy(&install_eclipse, bin.join("install-eclipse"));
            }
            let eclipse_useradd = self.eclipse_useradd(&musl);
            if eclipse_useradd.is_file() {
                let _ = fs::copy(&eclipse_useradd, bin.join("eclipse-useradd"));
            }
            let eclipse_bench = self.eclipse_bench(&musl);
            if eclipse_bench.is_file() {
                let _ = fs::copy(&eclipse_bench, bin.join("eclipse-bench"));
            }
            let clock_probe = self.clock_probe(&musl);
            if clock_probe.is_file() {
                let _ = fs::copy(&clock_probe, bin.join("clock-probe"));
            }
            self.install_signal_probe(&musl, &bin);
            self.install_usb_probe(&musl, &bin);
            let firefox_probe = self.firefox_probe(&musl);
            if firefox_probe.is_file() {
                let _ = fs::copy(&firefox_probe, bin.join("firefox-probe"));
            }
            let audio_probe = self.audio_probe(&musl);
            if audio_probe.is_file() {
                let _ = fs::copy(&audio_probe, bin.join("audio-probe"));
            }
            let drm_probe = self.drm_probe(&musl);
            if drm_probe.is_file() {
                let _ = fs::copy(&drm_probe, bin.join("drm-probe"));
            }
            let gfx_probe = self.gfx_probe(&musl);
            if gfx_probe.is_file() {
                let _ = fs::copy(&gfx_probe, bin.join("gfx-probe"));
            }
            // These two were refreshed only on a from-scratch build, so an
            // ordinary `make image` shipped whatever binary the rootfs already
            // had. A diagnostic that is a build behind is worse than none: it
            // answers questions about a system that no longer exists. Both are
            // cheap to copy and the from-scratch path does the same.
            let sdl_probe = self.eclipse_sdl_probe(&musl);
            if sdl_probe.is_file() {
                let _ = dir::rm(bin.join("eclipse-sdl-probe"));
                let _ = fs::copy(&sdl_probe, bin.join("eclipse-sdl-probe"));
            }
            let (dbusd, portald) = self.eclipse_dbusd();
            if dbusd.is_file() {
                let _ = dir::rm(bin.join("eclipse-dbusd"));
                let _ = fs::copy(&dbusd, bin.join("eclipse-dbusd"));
            }
            if portald.is_file() {
                let _ = dir::rm(bin.join("eclipse-portald"));
                let _ = fs::copy(&portald, bin.join("eclipse-portald"));
            }
            self.install_thread_tests(&dir);
            // The desktop's own clients. They were copied only on a
            // from-scratch build, so an ordinary `make image` kept whatever
            // lunarbar the rootfs was created with and never got the
            // binaries added after it -- lunarhunter, the day it arrived.
            self.install_lunar_tools(&bin);
            self.install_silversurfer(&bin);
            // INIT (PID 1): the Eclipse-native Rust init by default, with busybox
            // init as a resilient fallback. `install_busybox_init` runs first so
            // `/sbin/init` always resolves to *some* PID 1; `install_eclipse_init`
            // then repoints it at `eclipse-init` when its (best-effort) build is
            // available.
            Self::install_base_accounts(&dir);
            Self::install_hunter(&dir);
            self.install_busybox_init(&dir);
            self.install_eclipse_init(&dir, &musl);
            // Incremental builds (`make image` -> make(false)) take this early
            // path when the rootfs already exists — the common case, since a
            // checked-in rootfs/<arch> is present. It previously RETURNED before
            // desktop/Xorg install, so `startx` was never baked in on any but a
            // from-scratch (`clear`) build (observed: a freshly-built image
            // booting to "sh: startx: not found"). Refresh the desktop config
            // and (best-effort, idempotent) the Xorg package set here too, so an
            // ordinary rebuild picks them up. apk-add of already-present
            // packages is a no-op, so this is cheap on repeat builds.
            // /etc/profile carries the shell environment AND the serial
            // terminal-size probe; refresh it here too so a plain `make image`
            // (this incremental path) picks up changes, not only a from-scratch
            // build.
            Self::write_profile(&dir.join("etc"));
            Self::write_ntp(&dir);
            // Package tap: Ubuntu apt (default on x86_64/aarch64) or Alpine apk.
            if apt_rootfs::use_apt(self.0.name()) {
                apt_rootfs::bootstrap(&dir, self.0.name());
            } else {
                // Before the desktop stack: `xorg::install` runs apk against this
                // rootfs, and `/etc/apk/arch` is what decides which repository it
                // reads. An incremental build never wrote it, so a rootfs built
                // before this existed keeps resolving for the host's arch.
                Self::write_apk_arch(&dir.join("etc"), self.0.name());
                let keys = dir.join("etc").join("apk").join("keys");
                let n = Self::install_apk_keys(&keys, self.0.name());
                println!(
                    "apk keys: installed {n} key(s) into {} for {}",
                    keys.display(),
                    self.0.name()
                );
            }
            self.install_desktop_stack(&dir);
            // After apk so we can see whether the PulseAudio plugin/binary
            // landed, and so /etc/pulse wins over anything the package dropped.
            Self::write_asound_conf(&dir);
            Self::ensure_var_run(&dir);
            Self::write_pulse_conf(&dir);
            Self::ensure_pulse_accounts(&dir);
            // The icon has to be in the image before `os-release` claims
            // `LOGO=eclipse`: that field names an icon, and naming one that is
            // not there is what it said before this existed.
            logo::install(&dir, self.0.name(), self.1);
            // After the package tap too: `/usr/bin/localedef` and the locale
            // sources only exist once `locales` has been unpacked, and nothing
            // ran its postinst.
            locales::install(&dir, self.0.name());
            // LAST, and after the package tap: Ubuntu's `base-files` and
            // Alpine's `alpine-release` both carry an `/etc/os-release`, and
            // the last writer is the one the guest reads.
            os_release::install(&dir, self.0.name(), self.1);
            return;
        }
        // 准备最小系统需要的资源
        let musl = self.0.linux_musl_cross();
        let busybox = self.busybox(&musl);
        // 拷贝 apk
        let bin = dir.join("bin");
        let lib = dir.join("lib");
        dir::clear(&dir).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&lib).unwrap();

        if apt_rootfs::use_apt(self.0.name()) {
            let etc = dir.join("etc");
            fs::create_dir_all(&etc).unwrap();
            Self::write_resolv_conf(&etc);
            Self::write_hosts(&etc);
            apt_rootfs::bootstrap(&dir, self.0.name());
        } else {
            let apk = self.apk(&musl);
            // El binario del objetivo es para el sistema instalado y es
            // best-effort. La CONFIGURACION de apk la usa el apk del HOST.
            if apk.is_file() {
                fs::copy(&apk, bin.join("apk")).unwrap();
            } else {
                eprintln!(
                    "warning: no hay estatico de apk para {} ({}); el sistema instalado \
                     saldra sin /bin/apk, pero el cierre de paquetes se arma igual con el \
                     apk del host",
                    self.0.name(),
                    apk.display()
                );
            }
            {
                let etc = dir.join("etc");
                let etc_apk = etc.join("apk");
                fs::create_dir_all(&etc_apk).unwrap();
                fs::write(
                    etc_apk.join("repositories"),
                    "http://dl-cdn.alpinelinux.org/alpine/v3.24/main\nhttp://dl-cdn.alpinelinux.org/alpine/v3.24/community\n",
                )
                .unwrap();
                fs::write(etc_apk.join("world"), "").unwrap();
                Self::write_apk_arch(&etc, self.0.name());

                let keys_dst = etc_apk.join("keys");
                let n = Self::install_apk_keys(&keys_dst, self.0.name());
                if n == 0 {
                    eprintln!(
                        "warning: no Alpine apk keys found under tools/apk/keys or \
                         prebuilt/alpine-apk-keys — apk add will use --allow-untrusted"
                    );
                } else {
                    println!("apk keys: installed {n} key(s) into {}", keys_dst.display());
                }

                Self::write_resolv_conf(&etc);
                Self::write_hosts(&etc);
                let lib_apk = dir.join("lib").join("apk");
                fs::create_dir_all(&lib_apk).unwrap();
                let lib_apk_db = lib_apk.join("db");
                fs::create_dir_all(&lib_apk_db).unwrap();
                fs::write(lib_apk_db.join("installed"), "").unwrap();

                let var_lib = dir.join("var").join("lib");
                fs::create_dir_all(&var_lib).unwrap();
                #[cfg(unix)]
                let _ = unix::fs::symlink("../../lib/apk", var_lib.join("apk"));

                let var_cache_apk = dir.join("var").join("cache").join("apk");
                fs::create_dir_all(&var_cache_apk).unwrap();
            }
        }

        // 拷贝 busybox
        fs::copy(busybox, bin.join("busybox")).unwrap();

        let etc = dir.join("etc");
        fs::create_dir_all(&etc).unwrap();
        if !etc.join("resolv.conf").exists() {
            Self::write_resolv_conf(&etc);
        }
        if !etc.join("hosts").exists() {
            Self::write_hosts(&etc);
        }
        Self::write_profile(&etc);
        Self::write_ntp(&dir);
        Self::write_passwd(&etc, &dir);
        Self::write_console_configs(&etc, &dir);
        self.install_desktop_stack(&dir);
        Self::install_ca_certs(&dir);

        // /etc/machine-id — prevents dhcp_vendor "No such file or directory".
        // MUST be 32 lowercase hex digits: dbus validates it and refuses to
        // autolaunch ("Invalid machine ID in /etc/machine-id") on anything
        // else, which breaks GTK apps' session-bus fallback. "ec1195e0" is
        // hex-alphabet leetspeak for "eclipse0". Written only when absent, so
        // an existing (installer-generated) id is preserved — an installed
        // system with the old non-hex id needs it rewritten once by hand.
        let machine_id = etc.join("machine-id");
        if !machine_id.exists() {
            fs::write(&machine_id, b"ec1195e0ec1195e0ec1195e0ec1195e0\n").unwrap();
        }

        // /etc/hostname
        fs::write(etc.join("hostname"), b"Eclipse\n").unwrap();

        Self::write_asound_conf(&dir);
        Self::ensure_var_run(&dir);
        Self::write_pulse_conf(&dir);
        Self::ensure_pulse_accounts(&dir);

        // /etc/fstab — placeholders sustituidos por install-eclipse (sin mount
        // syscall en btrfs; EclipseFS lleva checksum de los datos, asi que alli
        // el instalador lo reescribe montando la particion).
        fs::write(
            etc.join("fstab"),
            Self::fstab_template(image::DiskFs::from_env()),
        )
        .unwrap();

        // 拷贝 libc.so
        let from = musl
            .join(format!("{}-linux-musl", self.0.name()))
            .join("lib")
            .join("libc.so");
        let to = lib.join(format!("ld-musl-{arch}.so.1", arch = self.0.name()));
        fs::copy(from, &to).unwrap();
        Ext::new(self.strip(&musl)).arg("-s").arg(to).invoke();
        // 为 busybox 支持的所有 applets 建立符号链接
        Self::ensure_busybox_applets(&bin);
        // Create standard pseudo-filesystem mount points
        let _ = fs::create_dir_all(dir.join("run"));
        let _ = fs::create_dir_all(dir.join("proc"));
        let _ = fs::create_dir_all(dir.join("sys"));
        let _ = fs::create_dir_all(dir.join("tmp"));
        let _ = fs::create_dir_all(dir.join("dev"));
        // Mount points referenced by /etc/fstab (EFI system partition, /home).
        // They must exist so `mount` (and the boot-time fstab processing) can
        // attach the filesystems there.
        let _ = fs::create_dir_all(dir.join("boot/efi"));
        let _ = fs::create_dir_all(dir.join("home"));
        // Ollama pull target on the eventual /home mount (not /tmp tmpfs).
        let _ = fs::create_dir_all(dir.join("home/moebius/.ollama/models"));
        let _ = fs::create_dir_all(dir.join("home/moebius/.ollama/tmp"));

        // udhcpc / udhcpc6 scripts — apply leases via `ip` (no ifconfig/route)
        let udhcpc_dir = dir.join("usr/share/udhcpc");
        fs::create_dir_all(&udhcpc_dir).unwrap();
        let udhcpc_script = udhcpc_dir.join("default.script");
        fs::write(
            &udhcpc_script,
            b"#!/bin/sh\n\
              # udhcpc (DHCPv4) script for Eclipse OS\n\
              RESOLV_CONF=/etc/resolv.conf\n\
              mask_to_prefix() {\n\
                case \"$1\" in\n\
                  255.255.255.255) echo 32 ;;\n\
                  255.255.255.254) echo 31 ;;\n\
                  255.255.255.252) echo 30 ;;\n\
                  255.255.255.248) echo 29 ;;\n\
                  255.255.255.240) echo 28 ;;\n\
                  255.255.255.224) echo 27 ;;\n\
                  255.255.255.192) echo 26 ;;\n\
                  255.255.255.128) echo 25 ;;\n\
                  255.255.255.0)   echo 24 ;;\n\
                  255.255.0.0)     echo 16 ;;\n\
                  255.0.0.0)       echo 8 ;;\n\
                  *)               echo 24 ;;\n\
                esac\n\
              }\n\
              case \"$1\" in\n\
                deconfig)\n\
                  ip link set dev \"$interface\" up 2>/dev/null\n\
                  ip -4 addr flush dev \"$interface\" 2>/dev/null\n\
                  ip -4 route del default dev \"$interface\" 2>/dev/null\n\
                  ;;\n\
                bound|renew)\n\
                  ip link set dev \"$interface\" up 2>/dev/null\n\
                  prefix=$(mask_to_prefix \"${subnet:-255.255.255.0}\")\n\
                  ip -4 addr flush dev \"$interface\" 2>/dev/null\n\
                  ip -4 addr add \"$ip/$prefix\" dev \"$interface\" 2>/dev/null\n\
                  if [ -n \"$router\" ]; then\n\
                    for r in $router; do\n\
                      ip -4 route del default 2>/dev/null\n\
                      ip -4 route add default via \"$r\" dev \"$interface\" 2>/dev/null\n\
                      break\n\
                    done\n\
                  fi\n\
                  if [ -n \"$dns\" ]; then\n\
                    : > \"$RESOLV_CONF\"\n\
                    for d in $dns; do\n\
                      echo \"nameserver $d\" >> \"$RESOLV_CONF\"\n\
                    done\n\
                  fi\n\
                  ;;\n\
                leasefail|nak)\n\
                  ;;\n\
              esac\n\
              exit 0\n",
        )
        .unwrap();
        let udhcpc6_script = udhcpc_dir.join("default6.script");
        fs::write(
            &udhcpc6_script,
            b"#!/bin/sh\n\
              # udhcpc6 (DHCPv6) script for Eclipse OS\n\
              RESOLV_CONF=/etc/resolv.conf\n\
              case \"$1\" in\n\
                deconfig)\n\
                  ip link set dev \"$interface\" up 2>/dev/null\n\
                  ;;\n\
                bound|renew)\n\
                  ip link set dev \"$interface\" up 2>/dev/null\n\
                  if [ -n \"$ipv6\" ]; then\n\
                    ip -6 addr del \"$ipv6/128\" dev \"$interface\" 2>/dev/null\n\
                    ip -6 addr add \"$ipv6/128\" dev \"$interface\" 2>/dev/null\n\
                  fi\n\
                  if [ -n \"$ipv6prefix\" ]; then\n\
                    ip -6 addr add \"$ipv6prefix\" dev \"$interface\" 2>/dev/null\n\
                  fi\n\
                  # Append IPv6 nameservers without truncating -> a full rewrite\n\
                  # would wipe DHCPv4 nameservers already written by udhcpc.\n\
                  if [ -n \"$dns\" ]; then\n\
                    touch \"$RESOLV_CONF\"\n\
                    for d in $dns; do\n\
                      grep -q \"^nameserver $d\\$\" \"$RESOLV_CONF\" 2>/dev/null \\\n\
                        || echo \"nameserver $d\" >> \"$RESOLV_CONF\"\n\
                    done\n\
                  fi\n\
                  ;;\n\
                leasefail|nak)\n\
                  ;;\n\
              esac\n\
              exit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&udhcpc_script, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&udhcpc6_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        // ALSO install them under /etc/udhcpc.
        //
        // `/usr/share/udhcpc/default.script` is where Alpine's busybox looks.
        // The busybox actually staged into this image is Ubuntu's, and its
        // compiled-in path is `/etc/udhcpc/default.script` — confirmed with
        // `strings /bin/busybox` in the running guest. So `udhcpc` found no
        // script at all and applied nothing:
        //
        //   udhcpc: lease of 10.0.2.15 obtained from 10.0.2.2   <- DHCP fine
        //   ip addr show eth0 -> inet 0.0.0.0/0                 <- never applied
        //
        // The interface stayed at 0.0.0.0, so every outbound connection failed
        // and `apk` had no network — while the link itself was up and the
        // e1000e driver was transmitting and receiving correctly. Passing
        // `-s /usr/share/udhcpc/default.script` explicitly configured the
        // address immediately, which is what isolated it.
        //
        // Install to BOTH paths rather than picking one: which busybox ends up
        // in the image is a staging decision that has changed before, and a
        // duplicated 1 KiB script is far cheaper than a silently networkless
        // system. `/etc` is already in LIVE_KEEP, so this ships in the QEMU
        // initramfs too.
        let etc_udhcpc = dir.join("etc/udhcpc");
        fs::create_dir_all(&etc_udhcpc).unwrap();
        for name in ["default.script", "default6.script"] {
            let dst = etc_udhcpc.join(name);
            fs::copy(udhcpc_dir.join(name), &dst).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        // openssl wrapper to busybox ssl_client
        let usr_sbin = dir.join("usr/sbin");
        fs::create_dir_all(&usr_sbin).unwrap();
        let openssl_script = usr_sbin.join("openssl");
        fs::write(
            &openssl_script,
            b"#!/bin/sh\n\
              if [ \"$1\" != \"s_client\" ]; then\n\
                echo \"openssl wrapper: command '$1' not supported\" >&2\n\
                exit 1\n\
              fi\n\
              shift\n\
              CONNECT=\"\"\n\
              SERVERNAME=\"\"\n\
              while [ $# -gt 0 ]; do\n\
                case \"$1\" in\n\
                  -connect)\n\
                    CONNECT=\"$2\"\n\
                    shift 2\n\
                    ;;\n\
                  -servername)\n\
                    SERVERNAME=\"$2\"\n\
                    shift 2\n\
                    ;;\n\
                  -quiet)\n\
                    shift 1\n\
                    ;;\n\
                  *)\n\
                    shift 1\n\
                    ;;\n\
                esac\n\
              done\n\
              if [ -z \"$CONNECT\" ]; then\n\
                echo \"openssl wrapper: missing -connect\" >&2\n\
                exit 1\n\
              fi\n\
              if [ -n \"$SERVERNAME\" ]; then\n\
                exec ssl_client -n \"$SERVERNAME\" \"$CONNECT\"\n\
              else\n\
                exec ssl_client \"$CONNECT\"\n\
              fi\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&openssl_script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        // 拷贝 nl_dump (netlink dump helper).
        // Do this AFTER symlink creation to ensure it's a real binary, not a BusyBox link.
        let nl_dump = self.nl_dump(&musl);
        if nl_dump.is_file() {
            let dst = bin.join("nl_dump");
            let _ = dir::rm(&dst);
            fs::copy(&nl_dump, &dst).unwrap();
        }

        // 拷贝 edhcpc (Eclipse DHCPv4 client).
        // This is a static, minimal DHCPv4 client that uses rtnetlink to apply IP/gw.
        let edhcpc = self.edhcpc(&musl);
        if edhcpc.is_file() {
            let dst = bin.join("edhcpc");
            let _ = dir::rm(&dst);
            fs::copy(&edhcpc, &dst).unwrap();
        }

        // DNS/hosts resolver shim (dynamic) + CLI helper.
        let eclipse_resolv = self.eclipse_resolv(&musl);
        if eclipse_resolv.is_file() {
            let _ = dir::rm(bin.join("eclipse-resolv"));
            fs::copy(&eclipse_resolv, bin.join("eclipse-resolv")).unwrap();
        }
        let libeclipse_dns = self.libeclipse_dns(&musl);
        if libeclipse_dns.is_file() {
            let _ = dir::rm(lib.join("libeclipse_dns.so"));
            fs::copy(&libeclipse_dns, lib.join("libeclipse_dns.so")).unwrap();
        }

        // labwc spawn race guard (delayed g_strfreev + NULL-safe execvp).
        // See tools/eclipse-spawnfix/spawnfix.c and the labwc wrapper.
        let libeclipse_spawnfix = self.libeclipse_spawnfix(&musl);
        if libeclipse_spawnfix.is_file() {
            let _ = dir::rm(lib.join("libeclipse_spawnfix.so"));
            fs::copy(&libeclipse_spawnfix, lib.join("libeclipse_spawnfix.so")).unwrap();
        }

        self.install_lunar_tools(&bin);
        self.install_silversurfer(&bin);

        // eclipse-dbusd: Eclipse's own D-Bus session bus (Rust, static musl).
        // The `eclipse-dbus` wrapper prefers Alpine's dbus-daemon when the
        // image has it and falls back to this, so an image built with no
        // package mirror in reach still gets a session bus -- which is what
        // SDL_Init(), GtkApplication and every RequestName-based single-
        // instance check need before they will run at all.
        let (dbusd, portald) = self.eclipse_dbusd();
        if dbusd.is_file() {
            let _ = dir::rm(bin.join("eclipse-dbusd"));
            fs::copy(&dbusd, bin.join("eclipse-dbusd")).unwrap();
        } else {
            eprintln!("warning: eclipse-dbusd not built; the session bus needs the dbus package");
        }

        // eclipse-portald: the XDG desktop portal, so that an application
        // whose "Open file" is a D-Bus call gets a dialog at all.
        if portald.is_file() {
            let _ = dir::rm(bin.join("eclipse-portald"));
            fs::copy(&portald, bin.join("eclipse-portald")).unwrap();
        } else {
            eprintln!("warning: eclipse-portald not built; there will be no file-chooser portal");
        }

        // wavplay: minimal OSS player for the HDA audio driver (`/dev/dsp*`).
        // `wavplay --tone` is the smoke test for HDMI audio on the RTX cards.
        let wavplay = self.wavplay();
        if wavplay.is_file() {
            let _ = dir::rm(bin.join("wavplay"));
            fs::copy(&wavplay, bin.join("wavplay")).unwrap();
        } else {
            eprintln!("warning: wavplay not built; /dev/dsp has no test client");
        }

        // eclipse-sdl-probe: SDL2/SDL3 smoke test + micro-bench for the desktop
        // (dlopens the apk-installed libSDL at run time, so it links against
        // libc only). `eclipse-sdl-probe` / `--sdl3 --surface` from a session
        // terminal say which video backend and render driver the SDL policy
        // produced and how fast they present. See tools/eclipse-sdl-probe.
        let sdl_probe = self.eclipse_sdl_probe(&musl);
        if sdl_probe.is_file() {
            let _ = dir::rm(bin.join("eclipse-sdl-probe"));
            fs::copy(&sdl_probe, bin.join("eclipse-sdl-probe")).unwrap();
        } else {
            eprintln!("warning: eclipse-sdl-probe not built; SDL has no smoke test in the image");
        }

        // 拷贝 install-eclipse
        let install_eclipse = self.install_eclipse(&musl);
        if install_eclipse.is_file() {
            let dst = bin.join("install-eclipse");
            let _ = dir::rm(&dst);
            fs::copy(&install_eclipse, &dst).unwrap();
        }

        // 拷贝 eclipse-useradd
        let eclipse_useradd = self.eclipse_useradd(&musl);
        if eclipse_useradd.is_file() {
            let dst = bin.join("eclipse-useradd");
            let _ = dir::rm(&dst);
            fs::copy(&eclipse_useradd, &dst).unwrap();
        }

        // 拷贝 eclipse-bench (CPU/mem/disk/process benchmark)
        let eclipse_bench = self.eclipse_bench(&musl);
        if eclipse_bench.is_file() {
            let dst = bin.join("eclipse-bench");
            let _ = dir::rm(&dst);
            fs::copy(&eclipse_bench, &dst).unwrap();
        }

        // clock-probe: the integrity of the clocks and the timers. Not a speed
        // figure: it reads two clocks against each other and says whether the
        // time the kernel reports is true. See tools/clock-probe.
        let clock_probe = self.clock_probe(&musl);
        if clock_probe.is_file() {
            let dst = bin.join("clock-probe");
            let _ = dir::rm(&dst);
            fs::copy(&clock_probe, &dst).unwrap();
        }

        // signal-probe: whether a signal puts the program back as it found it.
        self.install_signal_probe(&musl, &bin);
        self.install_usb_probe(&musl, &bin);

        // firefox-probe: the kernel interfaces Firefox depends on.
        let firefox_probe = self.firefox_probe(&musl);
        if firefox_probe.is_file() {
            let dst = bin.join("firefox-probe");
            let _ = dir::rm(&dst);
            fs::copy(&firefox_probe, &dst).unwrap();
        }

        // audio-probe: the audio layers Firefox's cubeb rests on, driven the
        // way alsa-lib / PulseAudio drive them, with an audible tone per layer.
        let audio_probe = self.audio_probe(&musl);
        if audio_probe.is_file() {
            let dst = bin.join("audio-probe");
            let _ = dir::rm(&dst);
            fs::copy(&audio_probe, &dst).unwrap();
        }

        // drm-probe: the DRM/KMS layers a compositor rests on, driven the way
        // wlroots/labwc/Mesa-GBM drive them (read-only; --scanout to display).
        let drm_probe = self.drm_probe(&musl);
        if drm_probe.is_file() {
            let dst = bin.join("drm-probe");
            let _ = dir::rm(&dst);
            fs::copy(&drm_probe, &dst).unwrap();
        }

        // gfx-probe: drm-probe's userspace companion. dlopens the installed
        // GBM/EGL/GLES/Vulkan/Wayland libraries and drives each layer of the
        // stack (GBM alloc, EGL bring-up, an off-screen GL render it reads back,
        // Vulkan enumeration, a live wl_shm round-trip). See tools/gfx-probe.
        let gfx_probe = self.gfx_probe(&musl);
        if gfx_probe.is_file() {
            let dst = bin.join("gfx-probe");
            let _ = dir::rm(&dst);
            fs::copy(&gfx_probe, &dst).unwrap();
        }

        // ecl-compute: SAXPY / GIOPS on the NVIDIA compute GPU via card1.
        let ecl_compute = self.ecl_compute(&musl);
        if ecl_compute.is_file() {
            let dst = bin.join("ecl-compute");
            let _ = dir::rm(&dst);
            fs::copy(&ecl_compute, &dst).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
            }
        } else {
            eprintln!("warning: ecl-compute not built; no userspace NVIDIA compute client");
        }

        // nvctl-probe: `nvidia-smi -L` through /dev/nvidiactl, no NVIDIA library.
        self.install_nvctl_probe(&musl, &bin);

        // ecl-vkcompute: SAXPY via Vulkan compute (NVK). Dynamic: musl static
        // cannot dlopen libvulkan.so.1.
        let ecl_vkcompute = self.ecl_vkcompute(&musl);
        if ecl_vkcompute.is_file() {
            let dst = bin.join("ecl-vkcompute");
            let _ = dir::rm(&dst);
            fs::copy(&ecl_vkcompute, &dst).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
            }
        } else {
            eprintln!("warning: ecl-vkcompute not built; no Vulkan compute canary in the image");
        }

        self.install_thread_tests(&dir);
        // INIT (PID 1): the Eclipse-native Rust init by default; busybox init
        // kept as a resilient fallback. busybox lays down `/sbin/init` -> busybox
        // (+ inittab + rcS) first, then `install_eclipse_init` repoints
        // `/sbin/init` -> `eclipse-init` when its (best-effort) build succeeds.
        Self::install_base_accounts(&dir);
        Self::install_hunter(&dir);
        self.install_busybox_init(&dir);
        self.install_eclipse_init(&dir, &musl);
        logo::install(&dir, self.0.name(), self.1);
        locales::install(&dir, self.0.name());
        // LAST, for the same reason as on the incremental path above.
        os_release::install(&dir, self.0.name(), self.1);
    }

    /// Instala tests freestanding de multihilo (thr3: repro de la barrier de sysbench).
    fn install_thread_tests(&self, rootfs: &Path) {
        if let Arch::X86_64 = self.0 {
            let thr3 = self.thread_test_thr3();
            if thr3.is_file() {
                let dst = rootfs.join("thr3");
                let _ = dir::rm(&dst);
                fs::copy(&thr3, &dst).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
        }
    }

    /// hunter's operator side: `/sbin/hunterctl` (refreshed on every build, so
    /// it never lags the kernel's control language), an example
    /// `/etc/hunter/policy.conf` (every line commented out: an image that boots
    /// with it behaves exactly as one without it), the antivirus database, its
    /// feeds, the scheduled scans and the service that runs them.
    fn install_hunter(rootfs: &Path) {
        let sbin = rootfs.join("sbin");
        fs::create_dir_all(&sbin).unwrap();
        // hunter-selftest beside it: the end-to-end checks of every part of
        // hunter, run on the system itself (scripts/qemu-hunter-selftest.sh
        // runs it in QEMU).
        for name in ["hunterctl", "hunter-selftest"] {
            let dst = sbin.join(name);
            let _ = dir::rm(&dst);
            fs::copy(PROJECT_DIR.join("tools/hunterctl").join(name), &dst).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let etc_hunter = rootfs.join("etc/hunter");
        fs::create_dir_all(&etc_hunter).unwrap();
        // Each written only when absent, so an operator's edits survive a
        // rebuild: the example policy, the (empty) antivirus database, the
        // signature feeds, the (commented out) keys their signatures are
        // checked with, the scheduled scans and the ports the network monitor
        // expects.
        for name in [
            "policy.conf",
            "signatures",
            "feeds",
            "keys",
            "schedule",
            "ports",
        ] {
            let dst = etc_hunter.join(name);
            if !dst.exists() {
                fs::copy(PROJECT_DIR.join("tools/hunterctl").join(name), &dst).unwrap();
            }
        }
        // Images built before the database shipped empty carry the EICAR
        // sample signatures, whose byte pattern flags every file that quotes
        // the test string: drop those two lines, keep the operator's.
        let sigs = etc_hunter.join("signatures");
        if let Ok(old) = fs::read_to_string(&sigs) {
            if old.lines().any(|l| l.ends_with(":EICAR-Test-File")) {
                let kept: String = old
                    .lines()
                    .filter(|l| !l.ends_with(":EICAR-Test-File"))
                    .map(|l| format!("{l}\n"))
                    .collect();
                fs::write(&sigs, kept).unwrap();
            }
        }
        fs::create_dir_all(etc_hunter.join("signatures.d")).unwrap();
        // The scheduled scans' daemon, for eclipse-init to supervise (busybox
        // init does not read this directory, so there it is inert). Rewritten
        // on every build like hunterctl: what it runs is decided by
        // /etc/hunter/schedule, which is the operator's.
        let svc_dir = rootfs.join("etc/eclipse/services");
        fs::create_dir_all(&svc_dir).unwrap();
        fs::write(
            svc_dir.join("hunter-scheduler.service"),
            b"# hunter's scheduled antivirus scans: /etc/hunter/schedule, history in\n\
              # /var/lib/hunter/scans. See `hunterctl schedule` and `hunterctl scan-history`.\n\
              exec = /sbin/hunterctl scheduler\n\
              type = respawn\n\
              log = /tmp/hunter-scheduler.log\n",
        )
        .unwrap();
    }

    /// Lay down the base `/etc/passwd` and `/etc/group` so name lookups resolve.
    ///
    /// Writes a minimal but standard set of system accounts. `/etc/passwd` is
    /// only created when absent (so it never clobbers accounts added later);
    /// `/etc/group` is created when absent, and otherwise just gets a `uucp`
    /// line appended if it lacks one — idempotent on incremental rebuilds.
    fn install_base_accounts(rootfs: &Path) {
        let etc = rootfs.join("etc");
        let _ = fs::create_dir_all(&etc);

        let passwd = etc.join("passwd");
        if !passwd.exists() {
            fs::write(
                &passwd,
                "root:x:0:0:root:/root:/bin/sh\n\
                 nobody:x:65534:65534:nobody:/:/bin/false\n\
                 _apt:x:100:65534:APT Sandbox User:/nonexistent:/bin/false\n",
            )
            .unwrap();
        }

        let group = etc.join("group");
        let base = "root:x:0:root\n\
                    bin:x:1:\n\
                    daemon:x:2:\n\
                    sys:x:3:\n\
                    adm:x:4:\n\
                    tty:x:5:\n\
                    disk:x:6:\n\
                    lp:x:7:\n\
                    wheel:x:10:root\n\
                    uucp:x:14:root\n\
                    nogroup:x:65533:\n\
                    nobody:x:65534:\n";
        match fs::read_to_string(&group) {
            Ok(existing) => {
                if !existing.lines().any(|l| l.starts_with("uucp:")) {
                    let mut updated = existing;
                    if !updated.ends_with('\n') {
                        updated.push('\n');
                    }
                    updated.push_str("uucp:x:14:root\n");
                    fs::write(&group, updated).unwrap();
                }
            }
            Err(_) => fs::write(&group, base).unwrap(),
        }
        Self::ensure_pulse_accounts(rootfs);
    }

    /// Wire up busybox `init` as the PID 1 base program.
    ///
    /// busybox already ships in the rootfs (with an `init` applet), so no
    /// package install / network is needed: this only lays down `/sbin/init`
    /// (-> `/bin/busybox`), a minimal `/etc/inittab`, and the `/etc/init.d/rcS`
    /// sysinit hook. The Eclipse kernel owns the virtual terminals (it spawns
    /// the per-VT shells itself), so the inittab has NO `getty`/`askfirst`
    /// lines — `init` runs the sysinit hook once and then reaps orphaned
    /// children as PID 1.
    fn install_busybox_init(&self, rootfs: &Path) {
        let etc = rootfs.join("etc");
        let _ = fs::create_dir_all(&etc);

        // /sbin/init -> /bin/busybox. busybox selects its applet from
        // basename(argv[0]), so exec'ing /sbin/init runs the `init` applet
        // regardless of the symlink target. (The kernel boots INIT=/sbin/init.)
        let sbin = rootfs.join("sbin");
        let _ = fs::create_dir_all(&sbin);
        let init_link = sbin.join("init");
        let _ = fs::remove_file(&init_link);
        #[cfg(unix)]
        {
            let _ = unix::fs::symlink("/bin/busybox", &init_link);
        }

        // Minimal inittab. NO getty/askfirst: the kernel already provides the
        // per-VT shells. busybox init runs the sysinit hook, then idles handling
        // Ctrl-Alt-Del / shutdown / restart while reaping orphaned children.
        fs::write(
            etc.join("inittab"),
            b"# Eclipse OS - busybox init. The kernel owns the virtual terminals\n\
              # (it spawns the per-VT shells), so there are NO getty lines here;\n\
              # init runs the sysinit hook once and then reaps orphaned children.\n\
              ::sysinit:/etc/init.d/rcS\n\
              ::ctrlaltdel:/bin/busybox reboot\n\
              ::shutdown:/bin/busybox swapoff -a\n\
              ::restart:/bin/busybox init\n",
        )
        .unwrap();

        // /etc/init.d/rcS — the sysinit hook. The kernel already mounts the root
        // fs, brings up the network and spawns the shells, so this is just a
        // place to start optional background services. Safe by default (no-op);
        // a commented example shows how to launch the seatd seat manager.
        let initd = etc.join("init.d");
        let _ = fs::create_dir_all(&initd);
        let rcs = initd.join("rcS");
        fs::write(
            &rcs,
            b"#!/bin/sh\n\
              # Eclipse OS sysinit hook (busybox init). Add boot-time services\n\
              # here; the kernel already handles root mount, networking and the\n\
              # per-TTY shells. Example - start the Wayland/X seat manager:\n\
              #   [ -x /usr/bin/seatd ] && /usr/bin/seatd >/dev/null 2>&1 &\n\
              exit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&rcs, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Compila thr3 con gcc del host (bare metal, sin libc).
    fn thread_test_thr3(&self) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("thread-tests");
        let source = dir.join("thr3.c");
        let executable = dir.join("thr3-metal");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling thr3 (sysbench barrier regression test)...");
        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new("gcc")
            .current_dir(&dir)
            .arg("-static")
            .arg("-no-pie")
            .arg("-nostdlib")
            .arg("-fno-stack-protector")
            .arg("-fno-builtin")
            .arg("-O1")
            .arg("-DQUICK_TEST")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            eprintln!("warning: failed to compile thr3");
        }
        executable
    }

    /// Escribe `/etc/apk/arch` con el arco OBJETIVO.
    ///
    /// Esto no es cosmetico: `/etc/apk/arch` es lo que apk lee para decidir
    /// **de que repositorio** baja. `/etc/apk/repositories` solo nombra la
    /// rama (`.../v3.24/main`); apk le pega el arco detras y pide
    /// `.../v3.24/main/<arco>/APKINDEX.tar.gz`. Sin este fichero cae a su arco
    /// compilado, que en una compilacion cruzada es **el del host**: un
    /// `make release` de aarch64 resolvia contra el indice de x86_64.
    /// Comprobado a mano: con `riscv64` aqui dentro, el mismo apk de x86_64
    /// pide `.../main/riscv64/APKINDEX.tar.gz`. Y `apk --initdb` NO lo escribe.
    fn write_apk_arch(etc: &Path, arch: &str) {
        let etc_apk = etc.join("apk");
        if fs::create_dir_all(&etc_apk).is_err() {
            return;
        }
        let _ = fs::write(etc_apk.join("arch"), format!("{arch}\n"));
    }

    fn write_resolv_conf(etc: &Path) {
        fs::write(
            etc.join("resolv.conf"),
            "nameserver 8.8.8.8\nnameserver 1.1.1.1\n",
        )
        .unwrap();
    }

    fn write_hosts(etc: &Path) {
        fs::write(
            etc.join("hosts"),
            b"127.0.0.1\tlocalhost\n\
::1\t\tlocalhost ip6-localhost ip6-loopback\n\
127.0.1.1\tEclipse\n",
        )
        .unwrap();
    }

    /// NTP config + a wrapper that waits for DHCP then runs an NTP client in
    /// the foreground. busybox `ntpd` first: it needs no privilege-separation
    /// user and no `chroot(2)` (this kernel has none). OpenNTPD is the
    /// fallback, run with the only flags OpenNTPD 6 still accepts — the old
    /// `ntpd -d -s -u root` line was a usage error (`-s` was removed in 6.0,
    /// `-u` never existed), so the service died in ~10 ms and init respawned
    /// it every 8 s forever, spamming the console. The `_ntp` account it
    /// requires is created by `ensure_pulse_accounts`.
    ///
    /// Not a single pipeline here ends in a reader that stops early. This
    /// script was the source of the two `SIGPIPE` deaths every boot logged
    /// (`[exit] pid=... (ip) killed by signal SIGPIPE` at ~2.7 s and
    /// `... (busybox)` at ~4.1 s): the route probe was
    /// `ip route | grep -q '^default'` and the applet probe was
    /// `busybox --list | grep -qx ntpd`. `grep -q` exits the instant it
    /// matches, so the writer's NEXT `write(2)` — musl buffers stdout in
    /// 1 KiB chunks, and `busybox --list` is ~2.5 KiB of applet names —
    /// lands on a pipe with no reader and gets `EPIPE` + `SIGPIPE`. The
    /// checks still worked (the pipeline's status is `grep`'s), but each one
    /// cost an `error!` line in the dmesg ring, which is where a real crash
    /// is supposed to stand out. Reading from a file instead means the
    /// writer always reaches EOF.
    fn write_ntp(rootfs: &Path) {
        let etc = rootfs.join("etc");
        let _ = fs::create_dir_all(&etc);
        fs::write(
            etc.join("ntpd.conf"),
            b"# Eclipse OS: NTP client (OpenNTPD). Servers after DHCP.\n\
              servers pool.ntp.org\n",
        )
        .unwrap();
        let localbin = rootfs.join("usr/local/bin");
        let _ = fs::create_dir_all(&localbin);
        fs::write(
            localbin.join("eclipse-ntpd"),
            b"#!/bin/sh\n\
              # Eclipse OS: wait for a default route, then NTP in foreground.\n\
              # Every probe reads from a file: `cmd | grep -q` kills `cmd`\n\
              # with SIGPIPE, and the kernel logs that death as an error.\n\
              LOG=/tmp/ntpd.log\n\
              exec >>\"$LOG\" 2>&1\n\
              i=0\n\
              while [ \"$i\" -lt 45 ]; do\n\
              \x20 if [ -n \"$(ip -4 route show default 2>/dev/null)\" ]; then break; fi\n\
              \x20 if ip route >/tmp/ntpd-routes 2>/dev/null \\\n\
              \x20\x20\x20 && grep -q \x27^default\x27 /tmp/ntpd-routes; then break; fi\n\
              \x20 sleep 2\n\
              \x20 i=$((i+1))\n\
              done\n\
              if /bin/busybox --list 2>/dev/null >/tmp/ntpd-applets \\\n\
              \x20\x20 && grep -qx ntpd /tmp/ntpd-applets; then\n\
              \x20 echo \"[eclipse-ntpd] busybox ntpd\"\n\
              \x20 exec /bin/busybox ntpd -n -N -p pool.ntp.org\n\
              fi\n\
              if [ -x /usr/sbin/ntpd ]; then\n\
              \x20 echo \"[eclipse-ntpd] openntpd (foreground)\"\n\
              \x20 exec /usr/sbin/ntpd -d\n\
              fi\n\
              echo \"eclipse-ntpd: no ntpd binary (apk add openntpd)\"\n\
              sleep 60\n\
              exit 1\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                localbin.join("eclipse-ntpd"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }

    fn write_profile(etc: &Path) {
        // La estrofa del logo va DETRAS de todo el profile, y por eso la
        // salida se concatena en vez de escribirse de una pieza: la sonda de
        // tamano del terminal acaba con un `\033[H` que lleva el cursor del
        // console grafico al origen, asi que lo pintado antes se dibuja y se
        // tapa. Ver `logo::profile_stanza`.
        fs::write(
            etc.join("profile"),
            [&b"export PATH=/usr/local/bin:/bin:/sbin:/usr/bin:/usr/sbin\n\
              export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt\n\
              export SSL_CERT_DIR=/etc/ssl/certs\n\
              export CURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt\n\
              # NOTE: /lib/libeclipse_dns.so is deliberately NOT in LD_PRELOAD.\n\
              # Until the kernel published AT_SECURE/AT_UID..AT_EGID in the\n\
              # auxv, musl ran every process in secure mode and silently\n\
              # DROPPED this preload -- so the whole system (apk, TLS, DNS)\n\
              # has always run without the shim actually loaded. The moment\n\
              # the auxv was fixed and the shim really injected everywhere,\n\
              # labwc froze within seconds of starting. Keep the system on\n\
              # its proven configuration; opt in per command if needed:\n\
              #   LD_PRELOAD=/lib/libeclipse_dns.so some-command\n\
              export HOME=/root\n\
              mkdir -p /root/.ollama 2>/dev/null\n\
              # Ollama models must not land on tmpfs. A multi-GB ollama pull\n\
              # into /tmp (or a home that is still ramfs) densified the kernel\n\
              # heap until epoll_pwait panicked on a 96-byte timer cell.\n\
              # Prefer /home/moebius on the installed disk; else /var/cache.\n\
              if [ -d /home/moebius ]; then\n\
              \x20 _ollama_root=/home/moebius/.ollama\n\
              elif [ -d /var/cache ]; then\n\
              \x20 _ollama_root=/var/cache/ollama\n\
              else\n\
              \x20 _ollama_root=/root/.ollama\n\
              fi\n\
              mkdir -p \"$_ollama_root/models\" \"$_ollama_root/tmp\" 2>/dev/null\n\
              export OLLAMA_MODELS=\"$_ollama_root/models\"\n\
              export OLLAMA_TMPDIR=\"$_ollama_root/tmp\"\n\
              # If llama-server dies with SIGILL (illegal instruction), the\n\
              # autodetection picked an AVX2/CUDA runner this CPU cannot run.\n\
              # Uncomment one of these, slowest->fastest that still works:\n\
              #   export OLLAMA_LLM_LIBRARY=cpu\n\
              #   export OLLAMA_LLM_LIBRARY=cpu_avx\n\
              #   export OLLAMA_LLM_LIBRARY=cpu_avx2\n\
              unset _ollama_root\n\
              export TERM=xterm-256color\n\
              # UI language. /etc/eclipse/locale (lang=es|en); default es.\n\
              # Never export LC_ALL -- it would freeze LANG for gettext/GTK.\n\
              _ecl_lang=es\n\
              if [ -r /etc/eclipse/locale ]; then\n\
              \x20 _ecl_lang=$(awk -F= '/^[[:space:]]*lang[[:space:]]*=/{gsub(/[[:space:]]/,\"\",$2); print $2; exit}' /etc/eclipse/locale)\n\
              fi\n\
              case \"$_ecl_lang\" in\n\
              \x20 en|EN|en_US) export LANG=en_US.UTF-8 LANGUAGE=en ;;\n\
              \x20 *) export LANG=es_ES.UTF-8 LANGUAGE=es:en ;;\n\
              esac\n\
              unset _ecl_lang\n\
              # Timezone from /etc/eclipse/timezone (country=ES|US or tz=Area/City).\n\
              # Default Spain. Independent of keyboard layout.\n\
              _ecl_tz=Europe/Madrid\n\
              if [ -r /etc/eclipse/timezone ]; then\n\
              \x20 _ecl_tz=$(awk -F= '/^[[:space:]]*tz[[:space:]]*=/{gsub(/[[:space:]]/,\"\",$2); print $2; exit}' /etc/eclipse/timezone)\n\
              fi\n\
              [ -n \"$_ecl_tz\" ] && export TZ=\"$_ecl_tz\"\n\
              unset _ecl_tz\n\
              # Default: wlroots/labwc must use the software (pixman) renderer.\n\
              # Otherwise wlroots tries GLES2/EGL then Vulkan, which either fails\n\
              # outright or starts on Mesa llvmpipe and becomes extremely slow\n\
              # because every frame is rendered and copied on CPU. It does not\n\
              # auto-fall back to pixman. See docs/README-drm.md.\n\
              #\n\
              # EXPERIMENTAL opt-out, gated on the SAME kernel cmdline flag as the\n\
              # kernel-side nouveau-uAPI surface (docs/README-nouveau-uapi.md): let\n\
              # wlroots attempt the real Vulkan/NVK renderer instead. That kernel\n\
              # uAPI has NEVER been exercised end-to-end by a real client -- expect\n\
              # it may fail to init cleanly (again, no fallback to pixman), or\n\
              # hang/crash partway through a frame. If labwc does not come up,\n\
              # reboot WITHOUT nvidia.nouveau_uapi on the cmdline.\n\
              #\n\
              # TWO conditions, like the kernel's own gate: the cmdline flag is a\n\
              # REQUEST and the NVIDIA GPU is the CAPABILITY. Only when BOTH hold\n\
              # is the kernel's nouveau uAPI actually on. Default session: GLES2\n\
              # on zink+NVK. Kill-switch: nvidia.wlr_pixman. Native Vulkan:\n\
              # nvidia.wlr_vulkan. nvidia.wlr_gles2 is accepted (= default).\n\
              # login(1) STRIPS arbitrary vars, so re-assert the whole policy here\n\
              # to match build_child_env and the wrapper.\n\
              if grep -q 'nvidia\\.nouveau_uapi' /proc/cmdline 2>/dev/null && \\\n\
              \x20\x20 [ \"$(tr -d '[:space:]' < /sys/class/drm/card0/device/vendor 2>/dev/null)\" = \"0x10de\" ]; then\n\
              \x20 if grep -q 'nvidia\\.wlr_pixman' /proc/cmdline 2>/dev/null; then\n\
              \x20\x20 export WLR_RENDERER=pixman\n\
              \x20\x20 export WLR_RENDERER_ALLOW_SOFTWARE=1\n\
              \x20\x20 export LIBGL_ALWAYS_SOFTWARE=1\n\
              \x20\x20 export SDL_RENDER_DRIVER=software\n\
              \x20\x20 export SDL_FRAMEBUFFER_ACCELERATION=0\n\
              \x20 elif grep -q 'nvidia\\.wlr_vulkan' /proc/cmdline 2>/dev/null; then\n\
              \x20\x20 export WLR_RENDERER=vulkan\n\
              \x20\x20 export WLR_DRM_NO_MODIFIERS=1\n\
              \x20\x20 # Vulkan compositor only: pin the loader to NVK's ICD.\n\
              \x20\x20 # mesa-vulkan-drivers also installs lavapipe (the CPU\n\
              \x20\x20 # driver, the floor under Xwayland's glamor); wlroots'\n\
              \x20\x20 # native Vulkan renderer walks EVERY physical device the\n\
              \x20\x20 # loader offers and keeps the one whose\n\
              \x20\x20 # VK_EXT_physical_device_drm major:minor matches the DRM\n\
              \x20\x20 # fd it opened. lavapipe has no DRM device, so it can\n\
              \x20\x20 # never BE that match -- what it costs is the diagnosis,\n\
              \x20\x20 # because every way the enumeration goes wrong arrives as\n\
              \x20\x20 # the one line 'Could not match drm and vulkan device'.\n\
              \x20\x20 for f in /usr/share/vulkan/icd.d/nouveau_icd.*.json; do\n\
              \x20\x20\x20 [ -r \"$f\" ] && { export VK_DRIVER_FILES=\"$f\" VK_ICD_FILENAMES=\"$f\"; break; }\n\
              \x20\x20 done\n\
              \x20\x20 export GALLIUM_DRIVER=zink\n\
              \x20\x20 export MESA_LOADER_DRIVER_OVERRIDE=zink\n\
              \x20\x20 export SDL_RENDER_DRIVER=opengles2\n\
              \x20\x20 export SDL_FRAMEBUFFER_ACCELERATION=opengles2\n\
              \x20 else\n\
              \x20\x20 # Default GPU path (and nvidia.wlr_gles2): GLES2 on zink+NVK.\n\
              \x20\x20 export WLR_RENDERER=gles2\n\
              \x20\x20 export WLR_DRM_NO_MODIFIERS=1\n\
              \x20\x20 export GALLIUM_DRIVER=zink\n\
              \x20\x20 export MESA_LOADER_DRIVER_OVERRIDE=zink\n\
              \x20\x20 export SDL_RENDER_DRIVER=opengles2\n\
              \x20\x20 export SDL_FRAMEBUFFER_ACCELERATION=opengles2\n\
              \x20 fi\n\
              elif grep -q 'nvidia\\.nouveau_uapi' /proc/cmdline 2>/dev/null && \\\n\
              \x20\x20 [ -r /sys/class/drm/card0/device/vendor ]; then\n\
              \x20 # flag but no NVIDIA (the GL=1 image under QEMU): software GL,\n\
              \x20 # the same stack as renderer=gl-sw -- labwc on GLES2/llvmpipe.\n\
              \x20 export WLR_RENDERER=gles2\n\
              \x20 export WLR_RENDERER_ALLOW_SOFTWARE=1\n\
              \x20 export LIBGL_ALWAYS_SOFTWARE=1\n\
              \x20 export SDL_RENDER_DRIVER=opengles2\n\
              \x20 export SDL_FRAMEBUFFER_ACCELERATION=opengles2\n\
              else\n\
              \x20 # no flag -> kernel uAPI off -> pixman compositor, and GL\n\
              \x20 # clients from this shell go llvmpipe (no hardware GL exists\n\
              \x20 # here: QEMU default, or an RTX booted without the flag).\n\
              \x20 export WLR_RENDERER=pixman\n\
              \x20 export WLR_RENDERER_ALLOW_SOFTWARE=1\n\
              \x20 export LIBGL_ALWAYS_SOFTWARE=1\n\
              \x20 export SDL_RENDER_DRIVER=software\n\
              \x20 export SDL_FRAMEBUFFER_ACCELERATION=0\n\
              fi\n\
              # SDL (sdl12-compat / SDL2 / SDL3) backends, renderer-independent:\n\
              # native Wayland first, X11 fallback (Xwayland here, Xorg under\n\
              # desktop=xorg -- the list makes ONE policy serve both sessions),\n\
              # and ALSA audio (which /etc/asound.conf routes through Pulse).\n\
              # Without the video pin SDL2 picks X11 whenever DISPLAY is set,\n\
              # i.e. always (DISPLAY=:0 pin), taking the Xwayland detour. The\n\
              # comma list needs SDL >= 2.24 (Alpine ships 2.30+); SDL3 reads the\n\
              # underscored names. Same policy as build_child_env and the labwc\n\
              # wrapper; the renderer half is in the block above.\n\
              export SDL_VIDEODRIVER=wayland,x11\n\
              export SDL_VIDEO_DRIVER=wayland,x11\n\
              export SDL_AUDIODRIVER=alsa\n\
              export SDL_AUDIO_DRIVER=alsa\n\
              # OpenAL: Pulse first (native libpulse), ALSA fallback (also\n\
              # routed through Pulse via asound.conf). PI-futexes are implemented,\n\
              # so pa_mutex_new() no longer aborts.\n\
              export ALSOFT_DRIVERS=pulse,alsa\n\
              export PULSE_SERVER=unix:/run/pulse/native\n\
              # Firefox: the native Wayland backend on every launch path. The\n\
              # eclipse-firefox wrapper (menu, .desktop) pins it too; this line\n\
              # covers a `firefox` typed in a terminal. The labwc environment\n\
              # file and eclipse-init's CHILD_ENV carry the same pin.\n\
              export MOZ_ENABLE_WAYLAND=1\n\
              # GTK from a terminal: the gdk-pixbuf loader registry the\n\
              # gtk-caches oneshot writes at boot (apk --no-scripts never wrote\n\
              # the system loaders.cache, so without this GTK decodes no image,\n\
              # 'Could not load a pixbuf from icon theme'), and no dconf. The\n\
              # labwc environment file and eclipse-init's CHILD_ENV carry the\n\
              # same two; the eclipse-firefox wrapper re-asserts them.\n\
              export GDK_PIXBUF_MODULE_FILE=/root/.cache/pixbuf-loaders.cache\n\
              export GSETTINGS_BACKEND=memory\n\
              # No session bus here: pin the address so libdbus never\n\
              # `autolaunch:`es (dbus-launch + X11 + dbus-daemon behind pipes,\n\
              # the chain SDL_Init walks first). That chain was NOT what hung\n\
              # gzdoom -- two kernel bugs were, see README-desktop.md -- but\n\
              # pinning it keeps a fork/exec/pipe detour out of every\n\
              # SDL_Init: with no daemon the connect is refused at once and\n\
              # apps carry on.\n\
              export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus\n\
              # GTK's file dialog is a portal call when GTK_USE_PORTAL=1. The\n\
              # stamp appears when eclipse-portald owns the name, so the var\n\
              # follows a portal that really serves. No wait here (the labwc\n\
              # wrapper has one): a login shell runs long after boot.\n\
              [ -e /run/user/0/eclipse-portal.ready ] && export GTK_USE_PORTAL=1\n\
              # wlroots' libinput backend aborts the whole compositor if it\n\
              # enumerates zero input devices ('libinput initialization failed,\n\
              # no input devices'). Without a running udevd to tag devices,\n\
              # libinput may find none even when /sys/class/input is populated.\n\
              # This flag lets the compositor start regardless; devices that ARE\n\
              # discovered still work, so it is safe to leave on permanently.\n\
              export WLR_LIBINPUT_NO_DEVICES=1\n\
              # Hardware cursor is now composited by the kernel: wlroots' legacy\n\
              # DRM backend calls drmModeSetCursor/MoveCursor, which the DRM\n\
              # scheme accepts and draws over each scanned-out frame. So do NOT\n\
              # set WLR_NO_HARDWARE_CURSORS -- letting wlroots use the hardware\n\
              # cursor path avoids re-rendering the whole scene on every pointer\n\
              # move (the whole point of a HW cursor).\n\
              # Pin wlroots to the console GPU's DRM node (card0 =\n\
              # nvidia-gpu-23:0.0). This box has TWO nvidia DRM cards (card0 =\n\
              # console GPU driving the physical monitor via the UEFI GOP\n\
              # framebuffer; card1 = the compute GPU we run kernels on). Without\n\
              # this, wlroots may enumerate both and bind a phantom connector on\n\
              # the compute GPU. Pinning card0 gives labwc a SINGLE logical\n\
              # output and keeps it off the compute GPU. Physical pixels always\n\
              # land on the GOP framebuffer via the kernel's software-KMS\n\
              # scanout regardless, so this is really about presenting one\n\
              # output, not about which port lights up.\n\
              export WLR_DRM_DEVICES=/dev/dri/card0\n\
              # Last-resort software-GL override, kept commented. The renderer\n\
              # block above already picks the right stack per the two-condition\n\
              # gate: hardware GL (vulkan+zink) only on NVIDIA+flag, software GL\n\
              # (gles2/llvmpipe) with the flag under QEMU, pixman otherwise. These\n\
              # two force Mesa's KMS software rasteriser explicitly, for a machine\n\
              # where even llvmpipe autodetect misbehaves -- not needed on any\n\
              # path validated so far.\n\
              #export GALLIUM_DRIVER=llvmpipe\n\
              #export MESA_LOADER_DRIVER_OVERRIDE=kms_swrast\n\
              # Runtime dir for the Wayland socket (created on demand, mode 0700).\n\
              export XDG_RUNTIME_DIR=/run/user/0\n\
              [ -d \"$XDG_RUNTIME_DIR\" ] || { mkdir -p \"$XDG_RUNTIME_DIR\" && chmod 0700 \"$XDG_RUNTIME_DIR\"; }\n\
              # --- serial terminal size detection --------------------------------\n\
              # The kernel console reports the FRAMEBUFFER size (e.g. 227x113 at a\n\
              # 2048x2048 mode). That is right for the on-screen graphic console\n\
              # but wrong for a serial viewer: full-screen apps (nano, less, top)\n\
              # then overflow. Ask the host terminal (QEMU -serial) for its size\n\
              # via cursor-position report and apply it with TIOCSWINSZ. Reject\n\
              # tiny answers (<2x2) and absurd sizes (>512): a bogus \\e[1;1R or\n\
              # an unanswered \\e[999;999H echo used to leave nano unusable.\n\
              # Only mark ECLIPSE_TTY_SIZED after stty succeeds so a failed probe\n\
              # can retry. With no serial reply this times out in ~0.3s (VTIME)\n\
              # and keeps the framebuffer size.\n\
              # Gated to VT 0 (or an unset ECLIPSE_VT, e.g. a pty terminal that\n\
              # answers CSI 6n instantly): the kernel deliberately never answers\n\
              # the cursor-position query on VTs (the SERIAL host terminal does),\n\
              # and serial mirrors only the ACTIVE VT -- which is VT 0 when the\n\
              # profiles run at boot. Probing on VTs 1-5 could never get a reply\n\
              # and just blocked each of those 5 shells 0.3 s + ~6 forks at boot.\n\
              if [ \"${ECLIPSE_VT:-0}\" = \"0\" ] \\\n\
              \x20  && [ -z \"${ECLIPSE_TTY_SIZED:-}\" ] && [ -t 0 ] && [ -t 1 ]; then\n\
              \x20 __sz_old=$(stty -g 2>/dev/null)\n\
              \x20 stty raw -echo min 0 time 3 2>/dev/null\n\
              \x20 printf '\\033[999;999H\\033[6n'\n\
              \x20 __sz=$(dd bs=32 count=1 2>/dev/null)\n\
              \x20 [ -n \"$__sz_old\" ] && stty \"$__sz_old\" 2>/dev/null\n\
              \x20 __sz=${__sz#*[}\n\
              \x20 __rows=${__sz%%;*}\n\
              \x20 __cols=${__sz#*;}; __cols=${__cols%%R*}\n\
              \x20 case \"$__rows$__cols\" in\n\
              \x20   ''|*[!0-9]*) : ;;\n\
              \x20   *) [ \"$__rows\" -gt 1 ] && [ \"$__cols\" -gt 1 ] \\\n\
              \x20        && [ \"$__rows\" -le 512 ] && [ \"$__cols\" -le 512 ] \\\n\
              \x20        && stty rows \"$__rows\" cols \"$__cols\" 2>/dev/null \\\n\
              \x20        && export ECLIPSE_TTY_SIZED=1 ;;\n\
              \x20 esac\n\
              \x20 unset __sz __sz_old __rows __cols\n\
              \x20 # Home the graphic cursor. The probe's \\e[999;999H is also parsed\n\
              \x20 # by the GOP console; VirtualBox UART is a file so CSI 6n never\n\
              \x20 # replies and the installer would otherwise look like a hung black\n\
              \x20 # screen (prompt parked on the last cell of a 2560x1600 mode).\n\
              \x20 printf '\\033[H'\n\
              fi\n"[..],
                logo::profile_stanza().as_bytes(),
            ]
            .concat(),
        )
        .unwrap();
    }

    /// Ensure the root user's home (`/root`) exists and that `/etc/passwd` and
    /// `/etc/group` carry a usable `root` entry. bash resolves `~`/the home
    /// directory via `getpwuid(geteuid())`, i.e. /etc/passwd — without a valid
    /// entry (and an existing home) it greets "I can't find my home directory!".
    /// Only writes the files when absent, so a package-provided passwd/group is
    /// left untouched.
    fn write_passwd(etc: &Path, rootfs: &Path) {
        // Root's home directory must exist for `cd ~` / login to succeed.
        let _ = fs::create_dir_all(rootfs.join("root"));

        let passwd = etc.join("passwd");
        if !passwd.exists() {
            fs::write(
                &passwd,
                b"root:x:0:0:root:/root:/bin/sh\n\
                  nobody:x:65534:65534:nobody:/:/sbin/nologin\n\
                  _apt:x:100:65534:APT Sandbox User:/nonexistent:/sbin/nologin\n",
            )
            .unwrap();
        }
        let group = etc.join("group");
        if !group.exists() {
            fs::write(
                &group,
                b"root:x:0:\n\
                  nogroup:x:65534:\n\
                  tty:x:5:\n\
                  video:x:28:\n",
            )
            .unwrap();
        }
    }

    /// Lay down configuration that console programs need to behave well:
    ///
    /// - `/root/.bashrc`: bash, unlike POSIX sh, does NOT read `/etc/profile`
    ///   for non-login interactive shells, so source it here to inherit the
    ///   system PATH, the DNS resolver shim (`LD_PRELOAD`) and the SSL cert
    ///   locations. Also sets a readable prompt.
    /// - `/etc/nanorc`: a minimal, option-only nano config (no `include` of the
    ///   syntax files, whose directives trip "Mistakes in '/etc/nanorc'" on some
    ///   nano builds). Written unconditionally so the OS default wins over a
    ///   package file; users can re-add `include` lines for syntax highlighting.
    fn write_console_configs(etc: &Path, rootfs: &Path) {
        let bashrc = rootfs.join("root").join(".bashrc");
        if let Some(parent) = bashrc.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(
            &bashrc,
            b"# Eclipse OS - bash config for the root account.\n\
              # bash ignores /etc/profile for non-login interactive shells, so\n\
              # source it here to inherit PATH, the DNS resolver shim and the\n\
              # SSL certificate locations.\n\
              [ -r /etc/profile ] && . /etc/profile\n\
              export PS1='\\[\\e[1;32m\\]Eclipse\\[\\e[0m\\]:\\[\\e[1;34m\\]\\w\\[\\e[0m\\]\\$ '\n\
              alias ll='ls -la'\n",
        )
        .unwrap();

        fs::write(
            etc.join("nanorc"),
            b"# Eclipse OS - minimal nano configuration.\n\
              # Option-only (no syntax `include`s) so it loads cleanly across\n\
              # nano versions. Add `include \"/usr/share/nano/*.nanorc\"` yourself\n\
              # for syntax highlighting.\n\
              set tabsize 4\n\
              set tabstospaces\n\
              set autoindent\n",
        )
        .unwrap();
    }

    /// The applets linked unconditionally, before `busybox --list` is consulted.
    ///
    /// This list is the FALLBACK, and the fallback is the whole story on every
    /// cross build: the complement below runs `busybox --list` on the *host*,
    /// and a busybox built for aarch64 or riscv64 does not run here. So an
    /// applet missing from this list has no link at all in those images, and
    /// every caller in the rootfs's own scripts hides the failure -- inside
    /// `$(...)` a missing command substitutes empty, and the rest redirect
    /// stderr to /dev/null. `applets_cover_what_the_generated_scripts_call` is
    /// the test that keeps this in step with those scripts.
    const BASE_APPLETS: &[&str] = &[
        "cat",
        "cp",
        "echo",
        "false",
        "grep",
        "gzip",
        "ip",
        "kill",
        "ln",
        "ls",
        "mkdir",
        "mv",
        "pidof",
        "ping",
        "ps",
        "pwd",
        "rm",
        "rmdir",
        "sh",
        "sleep",
        "stat",
        "tar",
        "touch",
        "true",
        "uname",
        "usleep",
        "watch",
        "ifconfig",
        "route",
        "udhcpc",
        "udhcpc6",
        "sed",
        "awk",
        "cmp",
        "diff",
        "logger",
        "hostname",
        "cut",
        "sort",
        "uniq",
        "head",
        "tail",
        "wc",
        "xargs",
        "find",
        "test",
        "expr",
        "id",
        "date",
        "env",
        "chmod",
        "chown",
        "vi",
        "top",
        "less",
        "ssl_client",
        "ssl_server",
        "wget",
        "traceroute",
        "traceroute6",
        "reboot",
        "halt",
        "poweroff",
        // Called by the scripts this module and `desktop.rs` write into the
        // rootfs. Each of these was missing, so on a cross build the line that
        // uses it quietly did nothing:
        "tr",       // /etc/profile: the card0 vendor check that picks the renderer
        "printf", // /etc/profile: the cursor query of the TTY-size probe, and the \e[H that homes the console
        "stty",   // /etc/profile: raw mode for that probe, and putting the tty back
        "dd",     // /etc/profile: reads the terminal's reply; eclipse-xkbmap writes /proc/kbd
        "basename", // eclipse-init: turns the socket path into WAYLAND_DISPLAY
        "dirname", // eclipse-xkbmap, eclipse-look: the mkdir -p of the file they upsert
        "pkill",  // eclipse-look, lunarrun: respawn the panel, refuse a second instance
        "setsid", // eclipse-init: detaches a oneshot so it does not hold up the panel
    ];

    /// Creates symlinks in `bin/` for every busybox applet.
    ///
    /// Called on both full and incremental builds so that a rootfs directory
    /// created before this feature existed (or after a partial build) still ends
    /// up with all applet symlinks in the final ext2 image.  Existing entries
    /// (real binaries like `nl_dump`) are never overwritten.
    fn ensure_busybox_applets(bin: &Path) {
        let mut applets: Vec<String> = Self::BASE_APPLETS.iter().map(|a| a.to_string()).collect();

        // Complement the list with `busybox --list` when it runs on the host.
        let busybox_bin = bin.join("busybox");
        if let Ok(out) = std::process::Command::new(&busybox_bin)
            .arg("--list")
            .output()
        {
            if out.status.success() {
                if let Ok(s) = String::from_utf8(out.stdout) {
                    for line in s.lines() {
                        let applet = line.trim().to_string();
                        if !applet.is_empty() && !applets.contains(&applet) {
                            applets.push(applet);
                        }
                    }
                }
            }
        }

        for applet in &applets {
            let link = bin.join(applet);
            if !link.exists() && !link.is_symlink() {
                #[cfg(unix)]
                let _ = std::os::unix::fs::symlink("busybox", &link);
            }
        }

        // `/usr/bin/env`, which is NOT one of the applet links above because
        // those all live in /bin.
        //
        // `#!/usr/bin/env <cmd>` is the single most common shebang there is --
        // it is how a script finds an interpreter through $PATH instead of
        // hardcoding its location -- and Eclipse had no /usr/bin/env at all,
        // only /bin/env. Every such script therefore failed to exec, and
        // (until the loader stopped tearing the address space down before it
        // could fail) took the calling process with it:
        //
        //   shebang: lookup interp "usr/bin/env" failed: EntryNotFound
        //   execve: LinuxElfLoader::load failed: ENOENT
        //   unhandled page fault ... -> SIGSEGV
        //
        // Alpine has no such split -- there /bin IS /usr/bin -- so nothing in
        // the apk closure supplies it either. A relative symlink to busybox,
        // which dispatches on argv[0], is the whole fix; apk may later install
        // a real coreutils `env` over it, which is equally fine.
        #[cfg(unix)]
        if let Some(rootfs) = bin.parent() {
            let usr_bin = rootfs.join("usr/bin");
            let _ = fs::create_dir_all(&usr_bin);
            let link = usr_bin.join("env");
            if !link.exists() && !link.is_symlink() {
                let _ = std::os::unix::fs::symlink("../../bin/busybox", &link);
            }
        }
    }

    const CA_PEM_URL: &str = "https://curl.se/ca/cacert.pem";

    /// Descarga (si hace falta) el bundle Mozilla y lo deja en `prebuilt/cacert.pem`.
    fn ensure_prebuilt_ca_pem() -> PathBuf {
        let prebuilt = PROJECT_DIR.join("prebuilt/cacert.pem");
        if prebuilt.is_file() {
            return prebuilt;
        }
        if let Some(parent) = prebuilt.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        println!(
            "Fetching CA bundle from {} -> {}",
            Self::CA_PEM_URL,
            prebuilt.display()
        );
        let status = std::process::Command::new("wget")
            .args(["-q", "--show-progress", "-O"])
            .arg(&prebuilt)
            .arg(Self::CA_PEM_URL)
            .status()
            .expect("failed to run wget for CA bundle");
        if !status.success() || !prebuilt.is_file() {
            panic!(
                "CA bundle missing: could not download {}\n\
                 Fix: wget -O prebuilt/cacert.pem {}\n\
                 or run: cargo xtask linux rootfs --arch <arch> --clear",
                prebuilt.display(),
                Self::CA_PEM_URL
            );
        }
        prebuilt
    }

    /// Copy Alpine apk RSA public keys into `dst` (`/etc/apk/keys`).
    ///
    /// Search order: in-tree `tools/apk/keys` (always shipped), then
    /// `prebuilt/alpine-apk-keys` (gitignored extras, e.g. the e1000e bench
    /// mirror key). Returns how many `.pub` files landed.
    fn install_apk_keys(dst: &Path, arch: &str) -> usize {
        fs::create_dir_all(dst).unwrap();
        let mut del_arco = 0usize;
        for base in [
            PROJECT_DIR.join("tools").join("apk").join("keys"),
            PROJECT_DIR.join("prebuilt").join("alpine-apk-keys"),
        ] {
            // Las sueltas valen para cualquier arco; las de `<base>/<arco>/`
            // son las de ESE arco y son las que hacen falta de verdad.
            for (src, es_del_arco) in [(base.clone(), false), (base.join(arch), true)] {
                let Ok(entries) = fs::read_dir(&src) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("pub") {
                        continue;
                    }
                    if fs::copy(&path, dst.join(entry.file_name())).is_ok() && es_del_arco {
                        del_arco += 1;
                    }
                }
            }
        }
        let total = fs::read_dir(dst)
            .ok()
            .map(|it| {
                it.flatten()
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("pub"))
                    .count()
            })
            .unwrap_or(0);
        // El caso que se escapaba: con claves de OTRO arco en el directorio,
        // `total` sale distinto de cero, asi que nadie pasa
        // `--allow-untrusted`, y apk se encuentra el APKINDEX del objetivo
        // firmado por una clave que no tiene. Dice «UNTRUSTED signature»,
        // **no instala nada** y la tirada sigue, porque el paso de paquetes es
        // best-effort. Por eso este aviso habla del ARCO, no del total.
        if del_arco == 0 && total > 0 {
            eprintln!(
                "warning: hay {total} clave(s) de apk pero ninguna de {arch}; el APKINDEX de \
                 {arch} saldra «UNTRUSTED signature» y no se instalara ni un paquete. \
                 Las de cada arco van en tools/apk/keys/{arch}/ (ver su README)"
            );
        }
        total
    }

    /// Instala certificados raíz en el rootfs (requerido para wget https).
    fn install_ca_certs(root: &Path) {
        let src = Self::ensure_prebuilt_ca_pem();
        let certs_dir = root.join("etc/ssl/certs");
        fs::create_dir_all(&certs_dir).unwrap();
        let bundle = certs_dir.join("ca-certificates.crt");
        fs::copy(&src, &bundle).unwrap();
        // Alias usado por varias herramientas.
        let alias = certs_dir.join("ca-bundle.crt");
        let _ = fs::remove_file(&alias);
        #[cfg(unix)]
        unix::fs::symlink("ca-certificates.crt", &alias).unwrap();
        #[cfg(not(unix))]
        fs::copy(&bundle, &alias).unwrap();
        println!(
            "Installed CA bundle ({} bytes) -> {}",
            fs::metadata(&bundle).map(|m| m.len()).unwrap_or(0),
            bundle.display()
        );
    }

    /// 将 musl 动态库放入 rootfs。
    pub fn put_musl_libs(&self) -> PathBuf {
        // 递归 rootfs
        self.make(false);
        let dir = self.0.linux_musl_cross();
        self.put_libs(&dir, dir.join(format!("{}-linux-musl", self.0.name())));
        dir
    }

    /// 指定架构的 rootfs 路径。
    #[inline]
    pub fn path(&self) -> PathBuf {
        PROJECT_DIR
            .join("rootfs")
            .join(format!("{}{}", self.0.name(), self.1.suffix()))
    }

    /// The rootfs's desktop, or its absence.
    ///
    /// Both paths through [`Self::make`] — the incremental one and the
    /// from-scratch one — ran this same sequence, copied word for word. What
    /// happens when one of the two copies falls behind is already written in
    /// this file: the incremental path used to RETURN before installing the
    /// desktop, and for several rounds a freshly built image booted to
    /// `sh: startx: not found`. One function, called from both.
    ///
    /// It is also the variant's seam: `minimal` installs none of this. It does
    /// not prune a desktop rootfs afterwards — it never has one — so there is
    /// no file list to keep in step with `DEFAULT_PACKAGES` every time a
    /// package is added.
    fn install_desktop_stack(&self, dir: &Path) {
        if !self.1.has_desktop() {
            println!(
                "minimal variant: no desktop (no labwc/Xorg, none of the Mesa/Firefox/XFCE \
                 package closure); the session is a console plus install-eclipse"
            );
            // Keyboard, locale and timezone are NOT the desktop's: `eclipse-init`
            // runs `eclipse-kbd --boot` on every boot, compositor or not, and a
            // console wants its layout and its local time just as much. Without
            // them the minimal image comes up with the compiled-in default
            // layout and no local time.
            desktop::install_console(dir);
            // `/etc/eclipse/desktop` is NOT written here: `install_eclipse_init`
            // is its single writer and takes the value from
            // `Variant::default_session()`. Two writers of one file is how the
            // value ended up depending on whether `cargo rootfs` or
            // `cargo image` ran last -- see the note there.
            return;
        }
        desktop::install(dir);
        // Bake the whole X.Org stack (server + libinput input driver + software
        // GL + xkb data + base fonts + xterm) into the rootfs so `startx` works
        // out of the box, instead of leaving it as a runtime `apk add` chore
        // that a fresh install / a fresh QEMU boot does not have. Best-effort:
        // an offline build just warns and ships without it.
        //
        // El apk del HOST, no el `bin/apk` del rootfs: ese es el estatico del
        // arco objetivo y en una compilacion cruzada no arranca. El arco
        // objetivo se le pasa aparte y viaja hasta el `--arch` de apk.
        if apk_closure_fits_image(self.0.name()) {
            // Don't call apk_host() on the apt path — that wget's Chimera for nothing.
            let apk = if apt_rootfs::use_apt(self.0.name()) {
                PathBuf::new()
            } else {
                Self::apk_host()
            };
            xorg::install(dir, &apk, self.0.name());
        } else {
            println!(
                "{}: sin el cierre de paquetes del escritorio (Mesa/Firefox/XFCE); no cabe en la \
                 imagen de este arco, ver apk_closure_fits_image",
                self.0.name()
            );
        }
        // Official Ollama + CUDA under /usr/local — opt-in (`ECLIPSE_OLLAMA_CUDA=1`).
        // Default image only ships install-ollama-cuda for the user to run later.
        if self.0.name() == "x86_64" {
            if ollama_cuda::wanted() {
                ollama_cuda::install(dir);
            } else {
                ollama_cuda::remove(dir, "Ollama not in the default image");
            }
        }
        // Needs the firefox package on disk, i.e. after xorg::install.
        desktop::write_firefox_default_prefs(dir);
        desktop::write_firefox_desktop_override(dir);
        // Needs the GTK/gsettings packages on disk, same reason.
        desktop::compile_gsettings_schemas(dir);
        // After apk too: it only downloads the IWADs when the `freedoom`
        // package did not land, which is not known until apk has run.
        desktop::ensure_freedoom_iwads(dir);
        xorg::report_freedoom(dir, "the rootfs");
    }

    /// Writes `/etc/eclipse/desktop`, the persistent session `eclipse-init`
    /// reads when the kernel command line carries no `desktop=`.
    fn write_desktop_session(rootfs: &Path, session: &str) {
        let etc_eclipse = rootfs.join("etc").join("eclipse");
        if let Err(e) = fs::create_dir_all(&etc_eclipse) {
            eprintln!("warning: could not create {etc_eclipse:?}: {e}");
            return;
        }
        let path = etc_eclipse.join("desktop");
        // With the trailing newline. `selected_desktop_from` takes the first
        // whitespace-separated token, so it makes no difference to init -- but a
        // config file that does not end in a newline is the one somebody later
        // appends to with `echo >>` and ends up with `nonelabwc`.
        match fs::write(&path, format!("{session}\n")) {
            Ok(()) => println!("desktop session: {session} ({})", path.display()),
            Err(e) => eprintln!("warning: could not write {path:?}: {e}"),
        }
    }

    /// 编译 busybox。
    fn busybox(&self, musl: impl AsRef<Path>) -> PathBuf {
        // 最终文件路径
        let target = self.0.target().join("busybox");
        // 如果文件存在，直接退出
        let executable = target.join("busybox");
        if executable.is_file() {
            return executable;
        }
        // 获得源码
        let source = REPOS.join("busybox");
        if !source.is_dir() {
            fetch_online!(source, |tmp| {
                // The upstream cgit endpoint intermittently drops shallow
                // clones in CI; use its GitHub mirror instead.
                Git::clone("https://github.com/vda-linux/busybox_mirror.git")
                    .dir(tmp)
                    .single_branch()
                    .depth(1)
                    .done()
            });
        }
        // 拷贝
        dir::rm(&target).unwrap();
        dircpy::copy_dir(source, &target).unwrap();
        // 配置
        Make::new().current_dir(&target).arg("defconfig").invoke();
        // Force static linking and disable PIE (Type EXEC is more stable in zCore)
        Ext::new("sed")
            .current_dir(&target)
            .arg("-i")
            .arg(
                "s/.*CONFIG_STATIC.*/CONFIG_STATIC=y/;\
                  s/.*CONFIG_PIE.*/CONFIG_PIE=n/;\
                  s/.*CONFIG_FEATURE_INDIVIDUAL.*/CONFIG_FEATURE_INDIVIDUAL=n/;\
                  s/.*CONFIG_FEATURE_SHARED_BUSYBOX.*/CONFIG_FEATURE_SHARED_BUSYBOX=n/;\
                  s/.*CONFIG_FEATURE_WGET_OPENSSL.*/CONFIG_FEATURE_WGET_OPENSSL=n/;\
                  s/.*CONFIG_FEATURE_WGET_HTTPS.*/CONFIG_FEATURE_WGET_HTTPS=y/;\
                  s/.*CONFIG_SSL_CLIENT.*/CONFIG_SSL_CLIENT=y/;\
                  s/.*CONFIG_FEATURE_IPV6.*/CONFIG_FEATURE_IPV6=y/;\
                  s/.*CONFIG_UDHCPC6.*/CONFIG_UDHCPC6=y/;\
                  s/.*CONFIG_FEATURE_UDHCPC6_RFC3646.*/CONFIG_FEATURE_UDHCPC6_RFC3646=y/;\
                  s/^# CONFIG_INIT is not set$/CONFIG_INIT=y/;\
                  s/^# CONFIG_FEATURE_USE_INITTAB is not set$/CONFIG_FEATURE_USE_INITTAB=y/;\
                  s/^# CONFIG_NTPD is not set$/CONFIG_NTPD=y/;\
                  s/^# CONFIG_FEATURE_NTPD_SERVER is not set$/CONFIG_FEATURE_NTPD_SERVER=n/",
            )
            .arg(".config")
            .invoke();
        Ext::new("sh")
            .current_dir(&target)
            .arg("-c")
            .arg("yes '' | make oldconfig")
            .invoke();

        // Pin the DHCP client dispatcher scripts explicitly.
        //
        // `udhcpc6 -i eth0` (without `-s`) uses CONFIG_UDHCPC6_DEFAULT_SCRIPT. Its
        // default value differs across busybox versions: older trees point it at the
        // IPv4 `default.script`, whose `deconfig` runs `ip -4 addr flush` (wiping the
        // IPv4 lease) and whose `bound` has no `ip -6 addr add` (so the DHCPv6 lease
        // is never applied). Force the IPv6 client to use `default6.script` and keep
        // the IPv4 client on `default.script` so both are deterministic. Done after
        // `oldconfig` so the (now enabled) UDHCPC6 symbols already exist in `.config`.
        Ext::new("sed")
            .current_dir(&target)
            .arg("-i")
            .arg(
                "s#^CONFIG_UDHCPC_DEFAULT_SCRIPT=.*#CONFIG_UDHCPC_DEFAULT_SCRIPT=\"/usr/share/udhcpc/default.script\"#;\
                  s#^CONFIG_UDHCPC6_DEFAULT_SCRIPT=.*#CONFIG_UDHCPC6_DEFAULT_SCRIPT=\"/usr/share/udhcpc/default6.script\"#",
            )
            .arg(".config")
            .invoke();
        Ext::new("sh")
            .current_dir(&target)
            .arg("-c")
            .arg("yes '' | make oldconfig")
            .invoke();

        // 编译
        let musl = musl.as_ref().canonicalize().unwrap();
        let cross_compile = format!(
            "{musl}/bin/{arch}-linux-musl-",
            musl = musl.display(),
            arch = self.0.name(),
        );

        Make::new()
            .current_dir(&target)
            .arg(format!("CROSS_COMPILE={cross_compile}"))
            .arg("LDFLAGS=-static -no-pie")
            .arg("EXTRA_LDFLAGS=-static -no-pie")
            .arg("CFLAGS=-fno-PIC -fno-PIE")
            .arg("EXTRA_CFLAGS=-fno-PIC -fno-PIE")
            .arg("CONFIG_STATIC=y")
            .arg("CONFIG_PIE=n")
            .invoke();
        // 裁剪
        Ext::new(self.strip(musl))
            .arg("-s")
            .arg(&executable)
            .invoke();
        executable
    }

    /// Descarga (o actualiza) el binario estático de apk-tools desde Chimera Linux.
    ///
    /// El binario se almacena en `tools/apk/apk-<arch>.static`, junto al código
    /// fuente del que ya no se compilará apk.  Se usa `wget --timestamping` (-N)
    /// para que la descarga solo ocurra si el servidor publica una versión más
    /// nueva que la copia local — comportamiento idéntico a los mirrors de Alpine.
    ///
    /// Arquitecturas disponibles en Chimera Linux que también soporta Eclipse OS:
    ///   x86_64 · aarch64 · riscv64
    fn apk(&self, _musl: &Path) -> PathBuf {
        Self::apk_for(self.0.name())
    }

    /// El apk que corre AQUI, en el host de la compilacion.
    ///
    /// `apk()` baja el binario del arco OBJETIVO, que es el que se copia al
    /// rootfs para que el sistema instalado tenga su propio apk. Pero el paso
    /// de paquetes de `xorg::install` ejecuta apk **en el host**, y un estatico
    /// de aarch64 no arranca en un x86_64. Para eso esta este: el binario del
    /// host, al que se le dice el arco objetivo con `--arch`, que es como
    /// Alpine misma cruza (`apk --arch aarch64 --root ...`).
    fn apk_host() -> PathBuf {
        Self::apk_for(std::env::consts::ARCH)
    }

    fn apk_for(arch: &str) -> PathBuf {
        const CHIMERA_APK_BASE: &str = "https://repo.chimera-linux.org/apk/latest";

        let filename = format!("apk-{arch}.static");
        let url = format!("{CHIMERA_APK_BASE}/{filename}");

        // Almacenar en tools/apk/ junto al código fuente.
        let apk_src_dir = PROJECT_DIR.join("tools").join("apk");
        let stored = apk_src_dir.join(&filename); // e.g. tools/apk/apk-x86_64.static

        println!("Checking apk ({arch}) against Chimera Linux repo...");
        // -N / --timestamping: sólo descarga si el servidor tiene versión más nueva.
        // -q: silencioso excepto errores.  -P: directorio destino.
        let status = Ext::new("wget")
            .arg("-N")
            .arg("-q")
            .arg("--show-progress")
            .arg("-P")
            .arg(&apk_src_dir)
            .arg(&url)
            .status();

        if status.success() {
            if stored.is_file() {
                // Asegurarse de que tiene permisos de ejecución.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = fs::metadata(&stored).unwrap().permissions();
                    if perms.mode() & 0o111 == 0 {
                        perms.set_mode(perms.mode() | 0o755);
                        fs::set_permissions(&stored, perms).unwrap();
                    }
                }
            }
        } else {
            eprintln!(
                "warning: no se pudo descargar/actualizar apk ({arch}) desde {url}. \
                 Se usará la copia local si existe."
            );
        }

        stored
    }

    /// 编译 nl_dump (static netlink dump helper).
    fn nl_dump(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("nl_dump");
        let executable = dir.join("nl_dump");
        let source = dir.join("nl_dump.c");
        // Rebuild if missing or if source is newer than the binary.
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling nl_dump...");
        let musl = musl.canonicalize().unwrap();
        let bin = musl.join("bin");
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let strip = self.strip(&musl);

        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile nl_dump");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// 编译 edhcpc (static DHCPv4 client for Eclipse OS).
    fn edhcpc(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("edhcpc");
        let executable = dir.join("edhcpc");
        let source = dir.join("edhcpc.c");
        // Rebuild if missing or if source is newer than the binary.
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling edhcpc...");
        let musl = musl.canonicalize().unwrap();
        let bin = musl.join("bin");
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let strip = self.strip(&musl);

        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile edhcpc");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// Is an already-built artifact usable for the architecture being built?
    ///
    /// Every cached-artifact gate below reuses a file that lives in the SOURCE
    /// tree (`tools/<name>/<name>`) when it is newer than its `.c`. That path
    /// carries no architecture, and the only test was the mtime -- so a build
    /// for one arch leaves its binary there and the NEXT build, for another
    /// arch, finds a file newer than the source and hands it straight to the
    /// rootfs. Nothing fails; the image just ships a binary for the wrong
    /// machine.
    ///
    /// That is not hypothetical. It is what `cargo verify-rootfs` caught the
    /// first time it ran in CI: `lib/libeclipse_dns.so` inside the aarch64
    /// rootfs was an x86_64 ELF, because the x86_64 job had built it first.
    /// The same hole was in all twenty-odd gates, and it is the same shape as
    /// the bug that shipped an arm64 ISO with no PID 1.
    ///
    /// A file that is not an ELF (a script, an asset) says nothing about the
    /// architecture, so it stays usable.
    fn cached_for_target(&self, artifact: &Path) -> bool {
        match verify::elf_machine(artifact) {
            Some(machine) => machine == verify::expected_machine(self.0),
            None => true,
        }
    }

    /// Build libeclipse_dns.so (LD_PRELOAD resolver shim).
    fn libeclipse_dns(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("eclipse-resolv");
        let lib = dir.join("libeclipse_dns.so");
        let source = dir.join("resolv.c");
        if lib.is_file() && source.is_file() && self.cached_for_target(&lib) {
            if let (Ok(lib_meta), Ok(src_meta)) = (fs::metadata(&lib), fs::metadata(&source)) {
                if let (Ok(lib_mtime), Ok(src_mtime)) = (lib_meta.modified(), src_meta.modified()) {
                    if lib_mtime >= src_mtime {
                        return lib;
                    }
                }
            }
        }

        println!("Compiling libeclipse_dns.so...");
        let musl = musl.canonicalize().unwrap();
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", musl.join("bin").display(), arch);
        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-shared")
            .arg("-fPIC")
            .arg("-O2")
            .arg("-o")
            .arg(&lib)
            .arg(&source)
            .status();
        if !status.success() {
            eprintln!("warning: failed to compile libeclipse_dns.so");
        }
        lib
    }

    /// Build eclipse-sdl-probe (tools/eclipse-sdl-probe): the SDL smoke test.
    /// DYNAMIC on purpose -- a static musl binary cannot `dlopen`, and the
    /// probe resolves libSDL2/libSDL3 at run time so the host needs no SDL
    /// headers or libraries. It links against libc (+dl, +m) only, which the
    /// rootfs's musl loader serves; libeclipse_spawnfix.so is the precedent
    /// for cross-built dynamic objects running on the Alpine musl there.
    fn eclipse_sdl_probe(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("eclipse-sdl-probe");
        let executable = dir.join("eclipse-sdl-probe");
        let source = dir.join("eclipse-sdl-probe.c");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling eclipse-sdl-probe...");
        let musl = musl.canonicalize().unwrap();
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", musl.join("bin").display(), arch);
        let strip = self.strip(&musl);
        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-O2")
            .arg("-Wall")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .arg("-ldl")
            .arg("-lm")
            .status();
        if !status.success() {
            eprintln!("warning: failed to compile eclipse-sdl-probe");
            return executable;
        }
        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// Build libeclipse_spawnfix.so (LD_PRELOAD for labwc's double-fork spawn).
    fn libeclipse_spawnfix(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("eclipse-spawnfix");
        let lib = dir.join("libeclipse_spawnfix.so");
        let source = dir.join("spawnfix.c");
        if lib.is_file() && source.is_file() && self.cached_for_target(&lib) {
            if let (Ok(lib_meta), Ok(src_meta)) = (fs::metadata(&lib), fs::metadata(&source)) {
                if let (Ok(lib_mtime), Ok(src_mtime)) = (lib_meta.modified(), src_meta.modified()) {
                    if lib_mtime >= src_mtime {
                        return lib;
                    }
                }
            }
        }

        println!("Compiling libeclipse_spawnfix.so...");
        let musl = musl.canonicalize().unwrap();
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", musl.join("bin").display(), arch);
        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-shared")
            .arg("-fPIC")
            .arg("-O2")
            .arg("-o")
            .arg(&lib)
            .arg(&source)
            .arg("-ldl")
            .arg("-lpthread")
            .status();
        if !status.success() {
            eprintln!("warning: failed to compile libeclipse_spawnfix.so");
        }
        lib
    }

    /// Build eclipse-resolv CLI (static).
    fn eclipse_resolv(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("eclipse-resolv");
        let executable = dir.join("eclipse-resolv");
        let source = dir.join("eclipse-resolv.c");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling eclipse-resolv...");
        let musl = musl.canonicalize().unwrap();
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", musl.join("bin").display(), arch);
        let strip = self.strip(&musl);
        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            eprintln!("warning: failed to compile eclipse-resolv");
            return executable;
        }
        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// 编译 install-eclipse (static installer for Eclipse OS).
    fn install_eclipse(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("install-eclipse");
        let executable = dir.join("install-eclipse");
        let source = dir.join("install-eclipse.c");
        // Rebuild if missing or if source is newer than the binary.
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling install-eclipse...");
        let musl = musl.canonicalize().unwrap();
        let bin = musl.join("bin");
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let strip = self.strip(&musl);
        let zlib = PROJECT_DIR.join("tools").join("zlib");
        let zlib_sources = [
            "adler32.c",
            "crc32.c",
            "inflate.c",
            "inffast.c",
            "inftrees.c",
            "zutil.c",
            "gzlib.c",
            "gzread.c",
            "gzclose.c",
        ];

        fs::create_dir_all(&dir).unwrap();
        let mut cmd = Ext::new(&cc);
        cmd.current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-D_LARGEFILE64_SOURCE=1")
            .arg("-DNO_GZCOMPRESS")
            .arg(format!("-I{}", zlib.display()))
            .arg("-o")
            .arg(&executable)
            .arg(&source);
        for src in zlib_sources {
            cmd.arg(zlib.join(src));
        }
        let status = cmd.status();
        if !status.success() {
            println!("Failed to compile install-eclipse");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// 编译 eclipse-useradd (static user/group manager for Eclipse OS).
    fn eclipse_useradd(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("eclipse-useradd");
        let executable = dir.join("eclipse-useradd");
        let source = dir.join("eclipse-useradd.c");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling eclipse-useradd...");
        let musl = musl.canonicalize().unwrap();
        let bin = musl.join("bin");
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let strip = self.strip(&musl);

        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile eclipse-useradd");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// Compile the eclipse-bench CPU/memory/disk/process benchmark (static musl)
    /// so it lands in the rootfs at /bin/eclipse-bench. Skips recompilation when
    /// the binary is newer than its single source file.
    fn eclipse_bench(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("eclipse-bench");
        let executable = dir.join("eclipse-bench");
        let source = dir.join("eclipse-bench.c");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling eclipse-bench...");
        let musl = musl.canonicalize().unwrap();
        let bin = musl.join("bin");
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let strip = self.strip(&musl);

        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile eclipse-bench");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// Compile `firefox-probe` (static musl) into `/bin/firefox-probe`: the
    /// kernel-interface checks Firefox depends on, each mirroring a pattern
    /// from Firefox's own source. Same mtime skip as `eclipse_bench`.
    fn firefox_probe(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("firefox-probe");
        let executable = dir.join("firefox-probe");
        let source = dir.join("firefox-probe.c");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling firefox-probe...");
        let musl = musl.canonicalize().unwrap();
        let bin = musl.join("bin");
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let strip = self.strip(&musl);

        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile firefox-probe");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// Compile `audio-probe` (static musl) into `/bin/audio-probe`: the sound
    /// counterpart of `firefox-probe`. It drives `/dev/dsp`, the raw ALSA PCM
    /// and control ABI of `/dev/snd` and the PulseAudio socket exactly as
    /// alsa-lib, cubeb and Pulse do, plays a 440 Hz tone through each playback
    /// layer, and reports which cubeb backend Firefox's `OpenCubeb()` would
    /// get. `-lm` for the sine (musl folds libm into libc; the flag is inert
    /// there and required by any other libc). Same mtime skip as
    /// `firefox_probe`.
    fn audio_probe(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("audio-probe");
        let executable = dir.join("audio-probe");
        let source = dir.join("audio-probe.c");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling audio-probe...");
        let musl = musl.canonicalize().unwrap();
        let bin = musl.join("bin");
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let strip = self.strip(&musl);

        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .arg("-lm")
            .status();
        if !status.success() {
            println!("Failed to compile audio-probe");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// Cross-compile `clock-probe` (tools/clock-probe): the integrity probe for
    /// the clocks and the timers. `-pthread` because the watchdog that keeps a
    /// timer which never fires from hanging the probe is a thread, and the
    /// per-thread CPU-clock check needs a second thread of its own. No libm.
    /// Best-effort: a missing musl gcc skips the tool, as everywhere here.
    fn clock_probe(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("clock-probe");
        let executable = dir.join("clock-probe");
        let source = dir.join("clock-probe.c");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling clock-probe...");
        let musl = musl.canonicalize().unwrap();
        let bin = musl.join("bin");
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let strip = self.strip(&musl);

        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-pthread")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile clock-probe");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// Cross-compile `drm-probe` (tools/drm-probe): the DRM/KMS layers a
    /// compositor rests on, driven bottom-up the way wlroots/labwc/Mesa-GBM
    /// drive them. Read-only by default (safe under a running compositor);
    /// `--scanout` modesets a test pattern onto the display. No libm.
    fn drm_probe(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("drm-probe");
        let executable = dir.join("drm-probe");
        let source = dir.join("drm-probe.c");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling drm-probe...");
        let musl = musl.canonicalize().unwrap();
        let bin = musl.join("bin");
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let strip = self.strip(&musl);

        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile drm-probe");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// Cross-compile `gfx-probe` (tools/gfx-probe): drm-probe's userspace
    /// companion. It dlopen()s the installed graphics libraries (GBM, EGL,
    /// GLESv2, Vulkan, Wayland) at run time, so — like eclipse-sdl-probe — it is
    /// a DYNAMIC binary linking only libc + libdl; a missing library just turns
    /// its section to SKIP. Best-effort: a missing musl gcc skips the tool.
    fn gfx_probe(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("gfx-probe");
        let executable = dir.join("gfx-probe");
        let source = dir.join("gfx-probe.c");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling gfx-probe...");
        let musl = musl.canonicalize().unwrap();
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", musl.join("bin").display(), arch);
        let strip = self.strip(&musl);
        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-O2")
            .arg("-Wall")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .arg("-ldl")
            .status();
        if !status.success() {
            eprintln!("warning: failed to compile gfx-probe");
            return executable;
        }
        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// Cross-compile `ecl-compute` (NVIDIA SAXPY/bench client) as a static musl
    /// binary for `/bin/ecl-compute`. Best-effort: a missing musl gcc just
    /// skips the tool.
    fn ecl_compute(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("ecl-compute");
        let executable = dir.join("ecl-compute");
        let source = dir.join("ecl-compute.c");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified()) {
                    if bin_mtime >= src_mtime {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling ecl-compute...");
        let musl = musl.canonicalize().unwrap();
        let bin = musl.join("bin");
        let arch = self.0.name();
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let strip = self.strip(&musl);

        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile ecl-compute");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    /// Cross-compile `ecl-vkcompute` (Vulkan SAXPY / NAK canary) as a *dynamic*
    /// musl binary. Static musl cannot `dlopen` libvulkan, same reason as
    /// `eclipse-sdl-probe`. Best-effort: missing musl gcc skips the tool.
    fn ecl_vkcompute(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("ecl-vkcompute");
        let executable = dir.join("ecl-vkcompute");
        let source = dir.join("ecl-vkcompute.c");
        let spv_h = dir.join("saxpy_spv.h");
        let gen = dir.join("gen_saxpy_spv.py");

        if gen.is_file() {
            let regen = match (fs::metadata(&gen), fs::metadata(&spv_h)) {
                (Ok(g), Ok(h)) => match (g.modified(), h.modified()) {
                    (Ok(gt), Ok(ht)) => gt > ht,
                    _ => false,
                },
                (Ok(_), Err(_)) => true,
                _ => false,
            };
            if regen {
                let _ = Ext::new("python3").arg(&gen).current_dir(&dir).status();
            }
        }

        let sources_newer = |bin_mtime: std::time::SystemTime| -> bool {
            for p in [&source, &spv_h] {
                if let Ok(m) = fs::metadata(p) {
                    if let Ok(t) = m.modified() {
                        if t > bin_mtime {
                            return true;
                        }
                    }
                }
            }
            false
        };
        if executable.is_file() && self.cached_for_target(&executable) {
            if let Ok(bin_meta) = fs::metadata(&executable) {
                if let Ok(bin_mtime) = bin_meta.modified() {
                    if !sources_newer(bin_mtime) {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling ecl-vkcompute...");
        let musl = musl.canonicalize().unwrap();
        let cc = format!(
            "{}/{}-linux-musl-gcc",
            musl.join("bin").display(),
            self.0.name()
        );
        let strip = self.strip(&musl);
        fs::create_dir_all(&dir).unwrap();
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-O2")
            .arg("-Wall")
            .arg("-fPIE")
            .arg("-pie")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .arg("-ldl")
            .status();
        if !status.success() {
            println!("Failed to compile ecl-vkcompute");
            return executable;
        }
        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }

    fn strip(&self, musl: impl AsRef<Path>) -> PathBuf {
        musl.as_ref()
            .join("bin")
            .join(format!("{}-linux-musl-strip", self.0.name()))
    }

    /// Rust target triple for the userspace musl build of `eclipse-init`.
    fn musl_rust_triple(&self) -> &'static str {
        match self.0 {
            Arch::X86_64 => "x86_64-unknown-linux-musl",
            Arch::Aarch64 => "aarch64-unknown-linux-musl",
            Arch::Riscv64 => "riscv64gc-unknown-linux-musl",
        }
    }

    /// True when the musl userspace tools have to be linked by a CROSS
    /// toolchain instead of the host's `cc`.
    fn needs_cross_linker(&self) -> bool {
        self.0.name() != std::env::consts::ARCH
    }

    /// The environment every one of the Rust musl userspace tools
    /// (`eclipse-init`, `eclipse-dbusd`, `wavplay`, `lunarbg`, `lunarbar`)
    /// is cross-built with. One place, because the five build functions had
    /// the same line copied five times and a sixth tool would have copied it
    /// again.
    ///
    /// `-C relocation-model=static` is the long-standing part: static,
    /// non-PIE, like the busybox/apk base in the rootfs.
    ///
    /// The LINKER is what was missing, and it only ever bit arm64. rustc
    /// drives the final link through `cc`, which on an x86_64 host is the
    /// host gcc and the host `/usr/bin/ld`. For
    /// `x86_64-unknown-linux-musl` that works by accident -- rust ships the
    /// crt objects and `libc.a` itself, and the host ld understands every
    /// flag -- so nobody noticed. For `aarch64-unknown-linux-musl` rustc
    /// adds `-Wl,--fix-cortex-a53-843419` (an AArch64-only erratum
    /// workaround) and the x86_64 ld rejects the option outright:
    ///
    /// ```text
    /// /usr/bin/ld: unrecognized option '--fix-cortex-a53-843419'
    /// ```
    ///
    /// Every tool here is best-effort, so each failure only printed a
    /// warning: `make image ARCH=aarch64` built a rootfs with NO
    /// eclipse-init (busybox stayed PID 1), no eclipse-dbusd, no wavplay and
    /// no lunarbar/lunarbg/lunarrun, and said so in warnings buried in a few
    /// thousand lines of `make release` output.
    ///
    /// So link with the toolchain that matches the target: the
    /// `<arch>-linux-musl-cross` gcc that [`Arch::linux_musl_cross`] already
    /// downloads for the rest of the rootfs. ONLY when the target arch
    /// differs from the host's, so a host-native build keeps the exact
    /// command line it has been building with all along.
    ///
    /// `CC_<triple>` comes along for cc-rs: a dependency with a build script
    /// that compiles C would otherwise reach for the host compiler too. It is
    /// the per-target spelling on purpose -- a bare `CC` would also be picked
    /// up by build scripts compiling for the HOST, which must keep using the
    /// host compiler.
    fn cross_build_env(&self) -> Vec<(String, String)> {
        let cc = self.needs_cross_linker().then(|| {
            // Downloads on first use, like every other consumer of it; by the
            // time a tool is built the rootfs has already asked for it.
            self.0
                .linux_musl_cross()
                .join("bin")
                .join(format!("{}-linux-musl-gcc", self.0.name()))
                .display()
                .to_string()
        });
        self.cross_build_env_with(cc.as_deref())
    }

    /// The pure half of [`Self::cross_build_env`], so the shape of the
    /// environment can be tested without a 100 MiB toolchain download.
    fn cross_build_env_with(&self, cc: Option<&str>) -> Vec<(String, String)> {
        let mut rustflags = String::from(STATIC_RUSTFLAGS);
        let mut env = Vec::new();
        if let Some(cc) = cc {
            rustflags.push_str(" -C linker=");
            rustflags.push_str(cc);
            // cc-rs looks up `CC_<triple>` with the dashes turned into
            // underscores. With the dashes left in, the variable is simply
            // never read and a C build script quietly uses the host compiler.
            env.push((
                format!("CC_{}", self.musl_rust_triple().replace('-', "_")),
                cc.to_string(),
            ));
        }
        env.push(("RUSTFLAGS".to_string(), rustflags));
        env
    }

    /// Cross-compile the Eclipse-native init (`tools/eclipse-init`, Rust) as a
    /// static, non-PIE musl binary and return its path. Best-effort: on failure
    /// the path may not exist and the caller keeps busybox init as PID 1.
    ///
    /// Built static + `relocation-model=static` (non-PIE) to match the rest of
    /// the rootfs (busybox/apk are static non-PIE ET_EXEC), which the Eclipse
    /// loader handles well.
    fn eclipse_init(&self, _musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("eclipse-init");
        let triple = self.musl_rust_triple();
        let executable = dir
            .join("target")
            .join(triple)
            .join("release")
            .join("eclipse-init");
        let source = dir.join("src").join("main.rs");
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            if let (Ok(b), Ok(s)) = (fs::metadata(&executable), fs::metadata(&source)) {
                if let (Ok(bm), Ok(sm)) = (b.modified(), s.modified()) {
                    if bm >= sm {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling eclipse-init (Rust, {triple})...");
        // Make sure the userspace musl target is available (best-effort).
        let _ = Ext::new("rustup")
            .arg("target")
            .arg("add")
            .arg(triple)
            .status();

        let mut cargo = Ext::new("cargo");
        cargo
            .current_dir(&dir)
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg(triple);
        // Static non-PIE, plus the cross linker when the host's `cc` cannot
        // link this target at all: see `cross_build_env`.
        for (k, v) in self.cross_build_env() {
            cargo.env(k, v);
        }
        let status = cargo.status();
        if !status.success() {
            eprintln!("warning: eclipse-init build failed; busybox init remains PID 1");
        }
        executable
    }

    /// Cross-compile lunarbg (`tools/lunarbg`, Rust) as a static musl binary
    /// and return its path. Best-effort: if the build fails the desktop
    /// autostart falls back to swaybg (see xtask/src/linux/desktop.rs).
    fn lunarbg(&self) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("lunarbg");
        let triple = self.musl_rust_triple();
        let executable = dir
            .join("target")
            .join(triple)
            .join("release")
            .join("lunarbg");
        // Rebuild when any source file is newer than the binary.
        let newest_src = ["src/main.rs", "src/scene.rs", "src/par.rs", "Cargo.toml"]
            .iter()
            .filter_map(|rel| fs::metadata(dir.join(rel)).ok()?.modified().ok())
            .max();
        if let (Ok(bin_meta), Some(src_mtime)) = (fs::metadata(&executable), newest_src) {
            if let Ok(bin_mtime) = bin_meta.modified() {
                if bin_mtime >= src_mtime && self.cached_for_target(&executable) {
                    return executable;
                }
            }
        }

        println!("Compiling lunarbg (Rust, {triple})...");
        let _ = Ext::new("rustup")
            .arg("target")
            .arg("add")
            .arg(triple)
            .status();
        let mut cargo = Ext::new("cargo");
        cargo
            .current_dir(&dir)
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg(triple);
        // Static non-PIE, plus the cross linker when the host's `cc` cannot
        // link this target at all: see `cross_build_env`.
        for (k, v) in self.cross_build_env() {
            cargo.env(k, v);
        }
        let status = cargo.status();
        if !status.success() {
            eprintln!("warning: lunarbg build failed; swaybg fallback remains");
        }
        executable
    }

    /// Persist contained kernel faults. The fault path records them in RAM
    /// (`kernel_hal::oops_log` -> /proc/oops) because it cannot touch a
    /// filesystem: interrupts are off, the heap may be the thing that just
    /// got smashed, and `oops` only proceeds with NO kernel lock held. This
    /// is the userspace half that turns that RAM record into a file that
    /// survives the console scrollback -- the same split Linux uses between
    /// the printk ring and syslogd.
    ///
    /// The chmod is NOT optional and is why this lives in its own function:
    /// the script shipped 0644 for its whole life, so `execve` of the service's
    /// `exec =` returned EACCES, the child `_exit(127)`ed in under a
    /// millisecond, and eclipse-init respawned it forever (backing off to
    /// MAX_BACKOFF, i.e. an `exit 127` line on the console every 8 s for the
    /// rest of the boot). The regression test below asserts the x bit.
    fn write_oopslog(localbin: &Path, svc_dir: &Path) {
        let script = localbin.join("eclipse-oopslog");
        fs::write(
            &script,
            b"#!/bin/sh\n\
              # Drain /proc/oops into /var/log/oops.log.\n\
              # Contained faults leave the machine RUNNING, so this can take its\n\
              # time; it only needs to beat the next reboot.\n\
              #\n\
              # /proc/oops is the WHOLE record since boot, not the new part, and\n\
              # this file is appended to across reboots. Writing the snapshot\n\
              # each time it changed meant a boot with three faults wrote the\n\
              # first one three times, and a reboot wrote the lot again under a\n\
              # fresh date header. Two captures of one bug came back with a\n\
              # [null-exec] block identical down to r14, from different boots,\n\
              # read as one fault. So: remember how many bytes have been\n\
              # written and append only what is past them.\n\
              OUT=/var/log/oops.log\n\
              mkdir -p /var/log 2>/dev/null\n\
              done_bytes=0\n\
              while :; do\n\
              \x20 cur=$(cat /proc/oops 2>/dev/null)\n\
              \x20 case \"$cur\" in ''|'# no contained kernel faults since boot') : ;; *)\n\
              \x20 \x20 now_bytes=$(printf '%s' \"$cur\" | wc -c)\n\
              \x20 \x20 if [ \"$now_bytes\" -lt \"$done_bytes\" ]; then done_bytes=0; fi\n\
              \x20 \x20 if [ \"$now_bytes\" -gt \"$done_bytes\" ]; then\n\
              \x20 \x20 \x20 { echo \"=== $(date 2>/dev/null || echo 'boot+?') ===\"; \\\n\
              \x20 \x20 \x20 \x20 printf '%s\\n' \"$cur\" | tail -c +$((done_bytes + 1)); } >> \"$OUT\"\n\
              \x20 \x20 \x20 done_bytes=$now_bytes\n\
              \x20 \x20 \x20 echo 'eclipse-oopslog: a kernel fault was contained; see /var/log/oops.log' > /dev/console 2>/dev/null\n\
              \x20 \x20 fi\n\
              \x20 esac\n\
              \x20 sleep 10\n\
              done\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fs::write(
            svc_dir.join("oopslog.service"),
            b"# Persist contained kernel faults (/proc/oops -> /var/log/oops.log).\n\
              exec = /usr/local/bin/eclipse-oopslog\n\
              type = respawn\n",
        )
        .unwrap();
    }

    /// The session bus, in the foreground so eclipse-init supervises it.
    ///
    /// Two implementations, in this order: Alpine's dbus-daemon when the
    /// image has it (DEFAULT_PACKAGES installs `dbus`), and Eclipse's own
    /// eclipse-dbusd otherwise. The fallback is not a toy -- it is what
    /// makes the bus exist on an image built with no package mirror in
    /// reach, and it is the one that can be tested against this kernel in
    /// QEMU. Both take the same argv, so the choice is a `command -v`.
    fn write_dbus_wrapper(localbin: &Path) {
        fs::write(
            localbin.join("eclipse-dbus"),
            b"#!/bin/sh\n\
              # Eclipse OS: D-Bus session bus for eclipse-init.\n\
              : \"${XDG_RUNTIME_DIR:=/run/user/0}\"; export XDG_RUNTIME_DIR\n\
              BUS=\"$XDG_RUNTIME_DIR/bus\"\n\
              [ -d \"$XDG_RUNTIME_DIR\" ] || { mkdir -p \"$XDG_RUNTIME_DIR\" && chmod 0700 \"$XDG_RUNTIME_DIR\"; }\n\
              # dbus validates /etc/machine-id and refuses to start on anything\n\
              # that is not 32 lowercase hex digits; /var/lib/dbus/machine-id is\n\
              # where its own code looks first.\n\
              [ -s /etc/machine-id ] || dbus-uuidgen > /etc/machine-id 2>/dev/null\n\
              mkdir -p /var/lib/dbus 2>/dev/null\n\
              [ -s /var/lib/dbus/machine-id ] || cp /etc/machine-id /var/lib/dbus/machine-id 2>/dev/null\n\
              # A socket left over from an earlier run of THIS boot makes bind()\n\
              # fail with EADDRINUSE even though nothing is listening. Only\n\
              # eclipse-init starts the bus, and it has already reaped the\n\
              # previous daemon before respawning this wrapper, so a socket\n\
              # found here is always stale.\n\
              rm -f \"$BUS\"\n\
              for d in /usr/bin /bin /usr/sbin /sbin; do\n\
              \x20 if [ -x \"$d/dbus-daemon\" ]; then\n\
              \x20 \x20 echo \"eclipse-dbus: $d/dbus-daemon on unix:path=$BUS\" > /dev/console 2>/dev/null\n\
              \x20 \x20 exec \"$d/dbus-daemon\" --session --nofork --nopidfile \\\n\
              \x20 \x20 \x20 --address=\"unix:path=$BUS\"\n\
              \x20 fi\n\
              done\n\
              for d in /usr/local/bin /usr/bin /bin; do\n\
              \x20 if [ -x \"$d/eclipse-dbusd\" ]; then\n\
              \x20 \x20 echo \"eclipse-dbus: $d/eclipse-dbusd on unix:path=$BUS\" > /dev/console 2>/dev/null\n\
              \x20 \x20 exec \"$d/eclipse-dbusd\" --session \\\n\
              \x20 \x20 \x20 --address=\"unix:path=$BUS\"\n\
              \x20 fi\n\
              done\n\
              # Neither: say so where it can be found. Without a bus, SDL and\n\
              # GTK still run (the address is pinned, so connect() fails fast\n\
              # with ECONNREFUSED instead of forking dbus-launch), but anything\n\
              # that needs a NAME on the bus will not.\n\
              MSG='eclipse-dbus: no dbus-daemon and no eclipse-dbusd -- there is\n\
              no session bus. Fix: apk add dbus, or rebuild the image so\n\
              /usr/local/bin/eclipse-dbusd is installed.'\n\
              echo \"$MSG\" > /dev/console 2>/dev/null || true\n\
              echo \"$MSG\" >&2\n\
              sleep 60\n\
              exit 127\n",
        )
        .unwrap();
        portal::write_wrapper(localbin);

        // D-Bus SYSTEM bus. PulseAudio runs `--system` (user-mode pulse
        // refuses uid 0), and in that mode its main.c connects to the system
        // bus. There was none, so every boot logged
        //   W: [pulseaudio] main.c: Unable to contact D-Bus:
        //   org.freedesktop.DBus.Error.FileNotFound: Failed to connect to
        //   socket /run/dbus/system_bus_socket: No such file or directory
        // and anything else that wanted a system name had nowhere to put it.
        //
        // This is a SECOND bus, not a view of the session one: names
        // registered here are invisible on unix:path=/run/user/0/bus and vice
        // versa, which is exactly how Linux works. It cannot be a symlink to
        // the session bus either -- /run/user/0 is 0700 root and `--system`
        // drops pulse to its own uid, so it could not reach it.
        fs::write(
            localbin.join("eclipse-dbus-system"),
            b"#!/bin/sh\n\
              # Eclipse OS: D-Bus system bus for eclipse-init, on the path\n\
              # libdbus compiles in as the default system address.\n\
              BUS=/run/dbus/system_bus_socket\n\
              mkdir -p /run/dbus\n\
              # /run/dbus itself must be traversable by every uid: the whole\n\
              # point of the system bus is that unprivileged services reach\n\
              # it. The socket's own mode is the daemon's business.\n\
              chmod 0755 /run/dbus 2>/dev/null || true\n\
              [ -s /etc/machine-id ] || dbus-uuidgen > /etc/machine-id 2>/dev/null\n\
              mkdir -p /var/lib/dbus 2>/dev/null\n\
              [ -s /var/lib/dbus/machine-id ] || cp /etc/machine-id /var/lib/dbus/machine-id 2>/dev/null\n\
              # Stale socket from an earlier run of THIS boot: bind() fails\n\
              # with EADDRINUSE although nothing listens. Same reasoning as\n\
              # eclipse-dbus -- init has already reaped the previous daemon.\n\
              rm -f \"$BUS\"\n\
              # dbus-daemon --system drops to the user its system.conf names\n\
              # and exits 1 if that name is not in /etc/passwd. On the console\n\
              # that failure is invisible --- the daemon's stderr goes to the\n\
              # service log --- and init just respawns it for ever, which is\n\
              # the loop this check exists to name out loud.\n\
              WANT=$(awk -F\x27[<>]\x27 \x27{for(i=1;i<NF;i++) if($i==\"user\"){print $(i+1); exit}}\x27 \\\n\
              \x20 /usr/share/dbus-1/system.conf 2>/dev/null)\n\
              USE_DAEMON=yes\n\
              if [ -n \"$WANT\" ] && ! grep -q \"^$WANT:\" /etc/passwd 2>/dev/null; then\n\
              \x20 # Create it here rather than only complaining. The image\n\
              \x20 # build lays this account down, but a rootfs built before\n\
              \x20 # that did not, and the cost of being wrong is a boot with\n\
              \x20 # no system bus at all. uid/gid 81 is what Alpine reserves.\n\
              \x20 #\n\
              \x20 # Both numbers and both names have to be free, or already\n\
              \x20 # this account\x27s, before either file is touched: handing dbus\n\
              \x20 # a gid that belongs to some other group would hand it that\n\
              \x20 # group\x27s files, and writing a passwd line whose gid has no\n\
              \x20 # group, or a group whose gid does not match, is worse than\n\
              \x20 # no account at all. On any conflict we touch nothing and\n\
              \x20 # fall back.\n\
              \x20 UID_FREE=no\n\
              \x20 grep -q \"^[^:]*:[^:]*:81:\" /etc/passwd 2>/dev/null || UID_FREE=yes\n\
              \x20 GID_OK=no\n\
              \x20 ADD_GROUP=no\n\
              \x20 if grep -q \"^$WANT:[^:]*:81:\" /etc/group 2>/dev/null; then\n\
              \x20 \x20 # The group is already there and already 81: reuse it.\n\
              \x20 \x20 GID_OK=yes\n\
              \x20 elif ! grep -q \"^$WANT:\" /etc/group 2>/dev/null \\\n\
              \x20 \x20 \x20 && ! grep -q \"^[^:]*:[^:]*:81:\" /etc/group 2>/dev/null; then\n\
              \x20 \x20 GID_OK=yes\n\
              \x20 \x20 ADD_GROUP=yes\n\
              \x20 fi\n\
              \x20 WRITE=no\n\
              \x20 if [ \"$UID_FREE\" = yes ] && [ \"$GID_OK\" = yes ] && [ -w /etc/passwd ]; then\n\
              \x20 \x20 WRITE=yes\n\
              \x20 \x20 if [ \"$ADD_GROUP\" = yes ] && [ ! -w /etc/group ]; then\n\
              \x20 \x20 \x20 WRITE=no\n\
              \x20 \x20 fi\n\
              \x20 fi\n\
              \x20 if [ \"$WRITE\" = yes ]; then\n\
              \x20 \x20 # Group first, so there is never a passwd line whose gid\n\
              \x20 \x20 # has no group behind it.\n\
              \x20 \x20 if [ \"$ADD_GROUP\" = yes ]; then\n\
              \x20 \x20 \x20 echo \"$WANT:x:81:\" >> /etc/group\n\
              \x20 \x20 fi\n\
              \x20 \x20 echo \"$WANT:x:81:81:dbus:/dev/null:/sbin/nologin\" >> /etc/passwd\n\
              \x20 \x20 echo \"eclipse-dbus-system: added the missing user \x27$WANT\x27 (81:81)\" \\\n\
              \x20 \x20 \x20 > /dev/console 2>/dev/null || true\n\
              \x20 fi\n\
              fi\n\
              if [ -n \"$WANT\" ] && ! grep -q \"^$WANT:\" /etc/passwd 2>/dev/null; then\n\
              \x20 M=\"eclipse-dbus-system: system.conf wants user \x27$WANT\x27 and\n\
              /etc/passwd has no such line, so dbus-daemon --system would exit 1\n\
              on every start; using eclipse-dbusd instead\"\n\
              \x20 echo \"$M\" > /dev/console 2>/dev/null || true\n\
              \x20 echo \"$M\" >&2\n\
              \x20 USE_DAEMON=no\n\
              fi\n\
              # Whatever dbus-daemon says on its way out has to reach the\n\
              # CONSOLE, because that is the only thing anybody reads during a\n\
              # boot: the service log lives in /tmp, so the first version of\n\
              # this service restarted for ever without a word about why. The\n\
              # exec below inherits this, and stdout still goes to the log.\n\
              if [ -w /dev/console ]; then\n\
              \x20 exec 2>/dev/console\n\
              fi\n\
              # Alpine's dbus-daemon first: --system brings the real policy\n\
              # from /usr/share/dbus-1/system.conf, which is what decides who\n\
              # may own a name here.\n\
              if [ \"$USE_DAEMON\" = yes ]; then\n\
              \x20 for d in /usr/bin /bin /usr/sbin /sbin; do\n\
              \x20 \x20 if [ -x \"$d/dbus-daemon\" ]; then\n\
              \x20 \x20 \x20 echo \"eclipse-dbus-system: $d/dbus-daemon on unix:path=$BUS\" > /dev/console 2>/dev/null\n\
              \x20 \x20 \x20 exec \"$d/dbus-daemon\" --system --nofork --nopidfile \\\n\
              \x20 \x20 \x20 \x20 --address=\"unix:path=$BUS\"\n\
              \x20 \x20 fi\n\
              \x20 done\n\
              fi\n\
              for d in /usr/local/bin /usr/bin /bin; do\n\
              \x20 if [ -x \"$d/eclipse-dbusd\" ]; then\n\
              \x20 \x20 echo \"eclipse-dbus-system: $d/eclipse-dbusd on unix:path=$BUS\" > /dev/console 2>/dev/null\n\
              \x20 \x20 exec \"$d/eclipse-dbusd\" --system\n\
              \x20 fi\n\
              done\n\
              MSG='eclipse-dbus-system: no dbus-daemon and no eclipse-dbusd --\n\
              there is no system bus. pulseaudio will log \"Unable to contact\n\
              D-Bus\" and carry on; anything that needs a system name will not.'\n\
              echo \"$MSG\" > /dev/console 2>/dev/null || true\n\
              echo \"$MSG\" >&2\n\
              sleep 60\n\
              exit 127\n",
        )
        .unwrap();
    }

    /// Copy `lunarbg` and the five `tools/lunarbar` binaries into `bin`,
    /// building them first when their sources are newer. Called from both
    /// `make` paths, from-scratch and incremental.
    fn install_lunar_tools(&self, bin: &Path) {
        // lunarbg: the native wallpaper client (Rust, static musl). Renders
        // the Eclipse night scene procedurally over wlr-layer-shell, replacing
        // swaybg and its gdk-pixbuf image-decoding dependency entirely.
        let lunarbg = self.lunarbg();
        if lunarbg.is_file() {
            let _ = dir::rm(bin.join("lunarbg"));
            fs::copy(&lunarbg, bin.join("lunarbg")).unwrap();
        } else {
            eprintln!("warning: lunarbg not built; autostart will fall back to swaybg");
        }

        // lunarbar: the native status/task bar (Rust, static musl). A two-bar
        // panel over wlr-layer-shell + wlr-foreign-toplevel-management, replacing
        // waybar and its GTK/D-Bus/gdk-pixbuf/fontconfig dependency chain.
        let LunarTools {
            bar: lunarbar,
            run: lunarrun,
            file: lunarfile,
            clip: lunarclip,
            hunter: lunarhunter,
        } = self.lunar_tools();
        if lunarbar.is_file() {
            let _ = dir::rm(bin.join("lunarbar"));
            fs::copy(&lunarbar, bin.join("lunarbar")).unwrap();
        } else {
            eprintln!("warning: lunarbar not built; autostart will fall back to waybar");
        }

        // lunarrun: the KRunner stand-in (Alt+Space / Alt+F2) and KDE's
        // Super+D, from the same package. There IS a session bus now
        // (`dbus.service`), but krunner still needs Qt, KF6 and the rest of
        // Plasma; this speaks wlr-layer-shell and
        // wlr-foreign-toplevel-management instead.
        if lunarrun.is_file() {
            let _ = dir::rm(bin.join("lunarrun"));
            fs::copy(&lunarrun, bin.join("lunarrun")).unwrap();
        } else {
            eprintln!("warning: lunarrun not built; Alt+Space will do nothing");
        }

        // lunarfile: the file browser behind Super+E, and -- through
        // `--pick` -- the file chooser the rest of the system has never had.
        // Dolphin, Nautilus and Thunar are all GTK/Qt applications with a
        // gdk-pixbuf, fontconfig and theme-engine chain behind them; this is
        // wlr-layer-shell and wl_shm, like its two siblings. Without it
        // `eclipse-files` falls back to a shell in $HOME, which is what
        // Super+E did before it existed.
        if lunarfile.is_file() {
            let _ = dir::rm(bin.join("lunarfile"));
            fs::copy(&lunarfile, bin.join("lunarfile")).unwrap();
        } else {
            eprintln!("warning: lunarfile not built; Super+E falls back to a terminal");
        }

        // lunarclip: the clipboard manager (the daemon behind
        // `lunarclip.service`) and the Super+V history picker. On Wayland the
        // selection belongs to the client that copied, so closing the window
        // you copied from empties the clipboard; a manager over
        // wlr-data-control is the only thing that fixes that, and Alpine's
        // answer (wl-clipboard + clipman + wofi) is a Go program plus a GTK
        // launcher that wants the session bus.
        if lunarclip.is_file() {
            let _ = dir::rm(bin.join("lunarclip"));
            fs::copy(&lunarclip, bin.join("lunarclip")).unwrap();
        } else {
            eprintln!("warning: lunarclip not built; there is no clipboard history");
        }

        // lunarhunter: the window for hunter (configuration, the event ring,
        // scans with a live count, quarantine, signature updates). The bar's
        // shield menu opens it, and falls back to a terminal running
        // hunterctl when it is missing.
        if lunarhunter.is_file() {
            let _ = dir::rm(bin.join("lunarhunter"));
            fs::copy(&lunarhunter, bin.join("lunarhunter")).unwrap();
        } else {
            eprintln!("warning: lunarhunter not built; the shield menu opens terminals");
        }
    }

    /// Best-effort host build of SilverSurfer into `bin/silversurfer`.
    ///
    /// Unlike lunarbar this is **dynamically** linked (Servo needs Mesa;
    /// media needs GStreamer). If `pkg-config --exists gstreamer-1.0` fails,
    /// the build drops `media-gstreamer` and keeps the engine. A missing
    /// binary is fine: `eclipse-browser` falls back to Firefox.
    fn install_silversurfer(&self, bin: &Path) {
        let dir = PROJECT_DIR.join("tools").join("SilverSurfer");
        let executable = dir.join("target").join("release").join("silversurfer");
        let newest_src = [
            newest_mtime_under(&dir.join("src")),
            fs::metadata(dir.join("Cargo.toml"))
                .ok()
                .and_then(|m| m.modified().ok()),
        ]
        .into_iter()
        .flatten()
        .max();
        let bin_mtime = fs::metadata(&executable)
            .ok()
            .and_then(|m| m.modified().ok());
        let needs_build = match (newest_src, bin_mtime) {
            (Some(src), Some(bin_m)) => src > bin_m,
            _ => true,
        };
        if needs_build {
            let media = Ext::new("pkg-config")
                .arg("--exists")
                .arg("gstreamer-1.0")
                .status()
                .success();
            if media {
                println!("Compiling silversurfer (host, media-gstreamer)...");
            } else {
                println!(
                    "Compiling silversurfer (host, engine only; install libgstreamer1.0-dev for media)..."
                );
            }
            let mut cargo = Ext::new("cargo");
            cargo
                .current_dir(&dir)
                .arg("build")
                .arg("--release")
                .arg("--manifest-path")
                .arg(dir.join("Cargo.toml"));
            if !media {
                cargo
                    .arg("--no-default-features")
                    .arg("--features")
                    .arg("engine");
            }
            let status = cargo.status();
            if !status.success() {
                eprintln!(
                    "warning: silversurfer build failed; eclipse-browser falls back to Firefox"
                );
            }
        }
        if executable.is_file() {
            let _ = dir::rm(bin.join("silversurfer"));
            if let Err(err) = fs::copy(&executable, bin.join("silversurfer")) {
                eprintln!("warning: could not install silversurfer: {err}");
            }
        } else {
            eprintln!(
                "warning: silversurfer not built; menu uses eclipse-browser → Firefox fallback"
            );
        }
    }

    /// Cross-compile the `tools/lunarbar` package (Rust) as static musl
    /// binaries and return all five: the panel, `lunarrun` (the KRunner-style
    /// launcher behind Alt+Space and Super+D), `lunarfile` (the file browser
    /// behind Super+E, and the file chooser `--pick` gives the rest of the
    /// system), `lunarclip` (the clipboard manager and its Super+V history)
    /// and `lunarhunter` (hunter's window, opened from the shield menu). One
    /// cargo invocation builds them, since they share a library.
    /// Best-effort: if the build fails the desktop falls back to waybar, the
    /// runner keybinds do nothing (their wrapper says so in
    /// /tmp/lunarrun.log), `eclipse-files` falls back to a terminal, there
    /// is no clipboard history and the shield menu opens terminals.
    fn lunar_tools(&self) -> LunarTools {
        let dir = PROJECT_DIR.join("tools").join("lunarbar");
        let triple = self.musl_rust_triple();
        let out = dir.join("target").join(triple).join("release");
        let executable = out.join("lunarbar");
        let runner = out.join("lunarrun");
        let browser = out.join("lunarfile");
        let clipboard = out.join("lunarclip");
        let hunter = out.join("lunarhunter");
        // Rebuild when any source file is newer than the binaries. The whole
        // `src/` tree is walked rather than listed: the list this replaced
        // missed `hunter.rs` and `hunter_menu.rs` the day they arrived, and a
        // module missing from it is a source change that silently never
        // reaches the image.
        let newest_src = [
            newest_mtime_under(&dir.join("src")),
            fs::metadata(dir.join("Cargo.toml"))
                .ok()
                .and_then(|m| m.modified().ok()),
        ]
        .into_iter()
        .flatten()
        .max();
        // ALL FIVE must exist and be current: a tree built before lunarclip
        // existed has an up-to-date lunarbar, lunarrun and lunarfile and no
        // clipboard at all, and the stale-binary check would happily ship
        // that. (That is exactly what happened the day lunarfile arrived.)
        let mtimes: Option<Vec<_>> = [&executable, &runner, &browser, &clipboard, &hunter]
            .iter()
            .map(|p| fs::metadata(p).ok()?.modified().ok())
            .collect();
        if let (Some(bins), Some(src_mtime)) = (mtimes, newest_src) {
            if bins.iter().all(|m| *m >= src_mtime) {
                return LunarTools {
                    bar: executable,
                    run: runner,
                    file: browser,
                    clip: clipboard,
                    hunter,
                };
            }
        }

        println!("Compiling lunarbar (Rust, {triple})...");
        let _ = Ext::new("rustup")
            .arg("target")
            .arg("add")
            .arg(triple)
            .status();
        let mut cargo = Ext::new("cargo");
        cargo
            .current_dir(&dir)
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg(triple);
        // Static non-PIE, plus the cross linker when the host's `cc` cannot
        // link this target at all: see `cross_build_env`.
        for (k, v) in self.cross_build_env() {
            cargo.env(k, v);
        }
        let status = cargo.status();
        if !status.success() {
            eprintln!(
                "warning: lunarbar/lunarrun/lunarfile/lunarclip/lunarhunter build failed; waybar fallback remains"
            );
        }
        LunarTools {
            bar: executable,
            run: runner,
            file: browser,
            clip: clipboard,
            hunter,
        }
    }

    /// `/etc/asound.conf`. When PulseAudio is in the image, ALSA `default`
    /// goes through the pulse plugin so ALSA-only apps multiplex on the
    /// daemon. The kernel PCM stays available as `eclipse_hw` / `hw:0,0` for
    /// the Pulse sink itself and for diagnostics. Without the plugin, `default`
    /// stays direct `hw:0,0` (the old single-client path).
    /// The `/etc/fstab` the installed system starts from: the installer
    /// fills in the devices. The disk lines say which filesystem the images
    /// are (`fs`); their columns are the ones `install-eclipse` looks for.
    fn fstab_template(fs: image::DiskFs) -> String {
        let t = format!("{:<7} ", fs.name());
        format!(
            "# /etc/fstab - generado por install-eclipse\n\
# <dispositivo>      <punto de montaje>  <tipo>  <opciones>       <dump>  <pass>\n\
__ECLIPSE_ROOT_DEV__  /                  {t}defaults          0  1\n\
__ECLIPSE_EFI_DEV___  /boot/efi          vfat    defaults,noatime  0  0\n\
__ECLIPSE_HOME_DEV__  /home              {t}defaults          0  0\n\
__ECLIPSE_SWAP_DEV__  none               swap    sw                0  0\n"
        )
    }

    pub(super) fn write_asound_conf(rootfs: &Path) {
        let etc = rootfs.join("etc");
        let _ = fs::create_dir_all(&etc);
        let conf = etc.join("asound.conf");
        if let Ok(existing) = fs::read_to_string(&conf) {
            if !existing.contains("eclipse-generated") {
                return; // user-customized: leave it alone
            }
        }
        let pulse_pcm = [
            "usr/lib/alsa-lib/libasound_module_pcm_pulse.so",
            "usr/lib/x86_64-linux-gnu/alsa-lib/libasound_module_pcm_pulse.so",
            "usr/lib/aarch64-linux-gnu/alsa-lib/libasound_module_pcm_pulse.so",
            "usr/lib/riscv64-linux-gnu/alsa-lib/libasound_module_pcm_pulse.so",
        ]
        .iter()
        .any(|rel| rootfs.join(rel).is_file());
        let have_pulse = rootfs.join("usr/bin/pulseaudio").is_file() && pulse_pcm;
        let body = if have_pulse {
            b"# eclipse-generated ALSA routing (delete this line to take ownership).\n\
              #\n\
              # default -> PulseAudio so aplay/mpg123/SDL-alsa share the HDA PCM.\n\
              # Pulse itself opens eclipse_hw (type hw) to avoid a plugin loop.\n\
              # Direct kernel access: `aplay -D eclipse_hw` (fails while Pulse holds it).\n\
              # Format conversion: `aplay -D plug x.wav`.\n\
              pcm.eclipse_hw {\n\
              \x20   type hw\n\
              \x20   card 0\n\
              \x20   device 0\n\
              }\n\
              pcm.plug {\n\
              \x20   type plug\n\
              \x20   slave.pcm \"eclipse_hw\"\n\
              }\n\
              pcm.!default {\n\
              \x20   type pulse\n\
              \x20   server unix:/run/pulse/native\n\
              }\n\
              ctl.!default {\n\
              \x20   type pulse\n\
              \x20   server unix:/run/pulse/native\n\
              }\n\
              pcm.pulse {\n\
              \x20   type pulse\n\
              \x20   server unix:/run/pulse/native\n\
              }\n\
              ctl.pulse {\n\
              \x20   type pulse\n\
              \x20   server unix:/run/pulse/native\n\
              }\n"
            .as_slice()
        } else {
            b"# eclipse-generated ALSA routing (delete this line to take ownership).\n\
              #\n\
              # hw:0,0 behind `plug`. PulseAudio was not in this image, so there\n\
              # is no mixer daemon; one playback client at a time.\n\
              #\n\
              # `default` is the plug-wrapped device, as on any Linux distro:\n\
              # the kernel PCM is S16LE stereo at the discrete HDA rates and\n\
              # nothing else, so a bare `type hw` default turned every mono or\n\
              # off-rate file into `cannot set hw params`. `plug` converts in\n\
              # alsa-lib (it is core, not a loadable module) and passes an\n\
              # already-matching stream through untouched.\n\
              # Unconverted kernel access: `aplay -D eclipse_hw x.wav`.\n\
              pcm.!default {\n\
              \x20   type plug\n\
              \x20   slave.pcm \"eclipse_hw\"\n\
              }\n\
              pcm.eclipse_hw {\n\
              \x20   type hw\n\
              \x20   card 0\n\
              \x20   device 0\n\
              }\n\
              pcm.plug {\n\
              \x20   type plug\n\
              \x20   slave.pcm \"eclipse_hw\"\n\
              }\n\
              ctl.!default {\n\
              \x20   type hw\n\
              \x20   card 0\n\
              }\n"
            .as_slice()
        };
        fs::write(&conf, body).unwrap();
    }

    /// PulseAudio system-instance config. Eclipse runs everything as root, so
    /// the daemon is `--system` (user-mode PulseAudio refuses uid 0) with a
    /// `pulse` account for the post-bind drop, and a unix socket at
    /// `/run/pulse/native`. The ALSA sink is mmap=0: the kernel PCM is
    /// RW-interleaved + SYNC_PTR only.
    fn write_pulse_conf(rootfs: &Path) {
        let pulse = rootfs.join("etc").join("pulse");
        let _ = fs::create_dir_all(&pulse);
        let marker = "# eclipse-generated";
        let write_if_ours = |path: &Path, body: &[u8]| {
            if let Ok(existing) = fs::read_to_string(path) {
                if !existing.contains(marker) {
                    return;
                }
            }
            fs::write(path, body).unwrap();
        };

        // A config written before the marker existed is frozen here FOREVER:
        // `write_if_ours` reads it as someone else's file and every later fix
        // to this generator stops at the disk. On hardware that kept a
        // `system.pa` whose module-native-protocol-unix line has no
        // `auth-cookie-enabled=0`:
        //
        //   E: module.c: Failed to load module "module-native-protocol-unix"
        //      (argument: "auth-anonymous=1 socket=/run/pulse/native"):
        //      initialization failed.
        //
        // Without that key the module loads-or-creates a cookie under a path
        // the `pulse` account cannot write, fails, and the daemon runs on with
        // NO socket: `pgrep pulseaudio` alive, /run/pulse/native absent, every
        // client (ALSA `default` is the pulse plugin) refused, and /dev/dsp
        // EBUSY because the daemon really does hold the cards. Nothing can
        // play, and no rebuild ever fixed it.
        //
        // A file that cannot start the daemon is not a config anyone chose to
        // own. Move such a file aside — only when it is recognisably a
        // descendant of this generator, by our own socket path — and let this
        // generation write a working one. The original stays as .bak.
        {
            let system_pa = pulse.join("system.pa");
            if let Ok(existing) = fs::read_to_string(&system_pa) {
                let ours_by_descent = existing.contains("socket=/run/pulse/native");
                let cannot_bind = existing.contains("module-native-protocol-unix")
                    && !existing.contains("auth-cookie-enabled=0");
                if !existing.contains(marker) && ours_by_descent && cannot_bind {
                    let bak = pulse.join("system.pa.bak");
                    let _ = fs::write(&bak, existing.as_bytes());
                    let _ = fs::remove_file(&system_pa);
                    println!(
                        "PulseAudio: /etc/pulse/system.pa predates the eclipse-generated marker \
                         and its module-native-protocol-unix has no auth-cookie-enabled=0, so the \
                         daemon could never bind /run/pulse/native. Replaced it (kept as system.pa.bak)."
                    );
                }
            }
        }

        write_if_ours(
            &pulse.join("daemon.conf"),
            b"# eclipse-generated PulseAudio daemon (delete this line to take ownership).\n\
              daemonize = no\n\
              system-instance = yes\n\
              exit-idle-time = -1\n\
              realtime-scheduling = no\n\
              high-priority = no\n\
              nice-level = 0\n\
              flat-volumes = no\n\
              enable-shm = yes\n\
              enable-memfd = yes\n\
              default-sample-format = s16le\n\
              # The kernel is a fixed-rate sink: the HDA link always runs at\n\
              # 48 kHz and hw:0,0 accepts any client rate, converting it into\n\
              # the ring with the kernel's polyphase resampler (~-90 dB) -- and\n\
              # a rate change no longer restarts the stream, so it never\n\
              # re-locks an HDMI/DP sink. `avoid-resampling` therefore hands a\n\
              # lone stream to the sink at its native rate (44.1 kHz for most\n\
              # music) instead of resampling it here with speex: the daemon\n\
              # only resamples when two streams at different rates play at\n\
              # once. `default-sample-rate` is what the sink opens at and what\n\
              # everything mixes to in that case. No `alternate-sample-rate`:\n\
              # that is the old, reprogram-the-link way of following a rate.\n\
              # To go back to the daemon resampling everything, set\n\
              # `avoid-resampling = no` (one line; the kernel side is then\n\
              # passthrough and byte-identical to before).\n\
              default-sample-rate = 48000\n\
              avoid-resampling = yes\n\
              default-sample-channels = 2\n\
              default-fragments = 4\n\
              default-fragment-size-msec = 25\n\
              # speex-float-1 is the lowest speex quality (chosen for battery on\n\
              # the phones PA ships on); with the sink resampling every non-48k\n\
              # stream (most music is 44.1 kHz), the resampler quality is what\n\
              # is left to hear. -5 is speex's high-quality tier: a flat\n\
              # passband to ~20 kHz and a stopband deep enough to be inaudible,\n\
              # at a few % of one core for stereo -- nothing on this desktop.\n\
              # Raise toward -7/-10 or soxr-hq (if the PA build has libsoxr) for\n\
              # more, lower to -1 only if PA ever starts arriving late.\n\
              resample-method = speex-float-5\n\
              log-target = stderr\n\
              log-level = info\n",
        );
        write_if_ours(
            &pulse.join("client.conf"),
            // `allow-autospawn-for-root` is not a client.conf key -- libpulse
            // logs "Unknown lvalue 'allow-autospawn-for-root'" once per line it
            // reads it, which is every time a client (Firefox) links libpulse,
            // spamming the terminal. `autospawn = no` already disables spawning
            // for everyone including root, so the line was redundant as well as
            // invalid. See the pulse client.conf(5) key list.
            b"# eclipse-generated PulseAudio client (delete this line to take ownership).\n\
              default-server = unix:/run/pulse/native\n\
              autospawn = no\n",
        );
        let pa = b"# eclipse-generated PulseAudio startup (delete this line to take ownership).\n\
              #!/usr/bin/pulseaudio -nF\n\
              #\n\
              # System instance over Eclipse's ALSA cards. No udev, no D-Bus,\n\
              # no capture: the kernel PCM is playback-only RW-interleaved.\n\
              #\n\
              # THE SOCKET IS LOADED BEFORE THE CARDS. module-alsa-sink touches\n\
              # hardware, and a card that wedges its load leaves the daemon alive,\n\
              # holding that PCM, with module-native-protocol-unix never reached --\n\
              # so /run/pulse/native never appears, every client gets\n\
              # 'Connection refused' (ALSA `default` is the pulse plugin) and\n\
              # /dev/dsp is EBUSY because the daemon does own the card. Observed\n\
              # exactly like that: `pgrep pulseaudio` alive, `ls /run/pulse/native`\n\
              # ENOENT. With the socket first, a wedged card costs one sink, not the\n\
              # whole daemon: `pactl list sinks` still answers and names the card\n\
              # that did not come up. The restore modules stay ahead of it -- they\n\
              # are pure state and must be in place before a client connects.\n\
              #\n\
              # auth-cookie-enabled=0: with auth-anonymous the cookie is never\n\
              # consulted, but the module still loads-or-creates one under\n\
              # $XDG_CONFIG_HOME/pulse (or ~/.config/pulse) and FAILS TO LOAD when it\n\
              # cannot -- the system instance runs as `pulse`, which cannot write\n\
              # there, so no socket existed and every libpulse client fell through\n\
              # (Firefox: 'OpenCubeb() failed to init cubeb').\n\
              .nofail\n\
              load-module module-device-restore\n\
              load-module module-stream-restore\n\
              load-module module-card-restore\n\
              load-module module-augment-properties\n\
              .fail\n\
              load-module module-native-protocol-unix auth-anonymous=1 auth-cookie-enabled=0 socket=/run/pulse/native\n\
              .nofail\n\
              load-module module-native-protocol-unix auth-anonymous=1 auth-cookie-enabled=0 socket=/run/user/0/pulse/native\n\
              # ALSA sinks are .nofail: a missing/busy card must not kill the\n\
              # daemon (eclipse-init would then crash-loop it). Only arguments\n\
              # module-alsa-sink accepts (src/modules/alsa/module-alsa-sink.c):\n\
              # one unknown key and pa_modargs rejects the WHOLE line -- that was\n\
              # mixer_device= and use_ucm= (module-alsa-card keys), which left the\n\
              # daemon with no sink at all. The mixer is found from the PCM's card.\n\
              load-module module-alsa-sink device=hw:0,0 mmap=0 tsched=0 ignore_dB=1 fragments=4 fragment_size=12000 sink_properties=device.description=Eclipse\n\
              load-module module-alsa-sink device=hw:1,0 mmap=0 tsched=0 ignore_dB=1 fragments=4 fragment_size=12000 sink_name=analog sink_properties=device.description=Analog\n\
              .fail\n\
              load-module module-always-sink\n\
              load-module module-intended-roles\n\
              # module-suspend-on-idle removed: on this kernel a suspended sink was\n\
              # not being resumed when a non-corked input attached (the resume runs\n\
              # in the sink IO thread, which under the desktop\'s load did not run it),\n\
              # so every stream played into a SUSPENDED sink and went silent. Keeping\n\
              # the sink out of SUSPENDED (it stays IDLE with the PCM open) lets a new\n\
              # stream play without a resume step.\n\
              load-module module-filter-heuristics\n\
              load-module module-filter-apply\n";
        write_if_ours(&pulse.join("system.pa"), pa);
        // The same script at a path that is ALWAYS ours, whatever the user has
        // done to system.pa. `eclipse-pulseaudio` falls back to it (pulseaudio
        // -n --file=) when the config in place cannot bind the socket, so an
        // installed system that never re-runs this generator still gets a
        // daemon clients can reach. See the unfreeze note above.
        fs::write(pulse.join("system.pa.eclipse"), pa).unwrap();
        write_if_ours(&pulse.join("default.pa"), pa);
        let _ = fs::create_dir_all(rootfs.join("var/lib/pulse"));
        let _ = fs::create_dir_all(rootfs.join("var/run/pulse"));
    }

    /// `/var/run` must be the FHS symlink to `/run` (`var/run -> ../run`), as
    /// on Alpine and every modern distribution. Two separate directories were
    /// being created instead (`run` here, `var/run/pulse` in
    /// `write_pulse_conf`), and PulseAudio in `--system` mode puts its socket
    /// at its compiled-in `/var/run/pulse/native`, ignoring
    /// `PULSE_RUNTIME_PATH`. Every client -- `client.conf`'s
    /// `default-server = unix:/run/pulse/native`, Firefox's libpulse, the boot
    /// chime, `pactl`, `audio-probe` -- looked in `/run/pulse/native`, found
    /// nothing, and cubeb reported `OpenCubeb() failed to init cubeb` with the
    /// daemon alive the whole time (`audio-probe` showed the kernel PCM path
    /// entirely healthy and only the socket missing). With one directory
    /// behind both paths the socket lands where it is looked for.
    ///
    /// Idempotent, and it replaces a real `var/run` directory left by an
    /// older build (its only content is runtime state). `run` is created
    /// first so the link never dangles for a `create_dir_all` that goes
    /// through it. Called on BOTH rootfs paths before `write_pulse_conf`.
    fn ensure_var_run(rootfs: &Path) {
        let run = rootfs.join("run");
        let var = rootfs.join("var");
        let var_run = var.join("run");
        let _ = fs::create_dir_all(&run);
        let _ = fs::create_dir_all(&var);
        if var_run.is_symlink() {
            // Only the exact FHS target counts: a link to anywhere else
            // (`../../tmp`, a dangling target) would still make
            // `write_pulse_conf` create pulse/ through it, somewhere other
            // than /run, and the socket mismatch this helper exists to end
            // would be back under a symlink instead of a directory.
            if fs::read_link(&var_run)
                .map(|t| t == Path::new("../run"))
                .unwrap_or(false)
            {
                return;
            }
            let _ = fs::remove_file(&var_run);
        } else if var_run.is_dir() {
            let _ = fs::remove_dir_all(&var_run);
        }
        if let Err(e) = unix::fs::symlink("../run", &var_run) {
            eprintln!("warning: could not create var/run -> ../run symlink: {e}");
        }
    }

    /// `pulse` / `pulse-access` / `audio` accounts. apk `--no-scripts` never
    /// runs the PulseAudio post-install, and `--system` drops to user `pulse`
    /// after binding the socket — without these lines the daemon exits.
    fn ensure_pulse_accounts(rootfs: &Path) {
        let etc = rootfs.join("etc");
        let _ = fs::create_dir_all(&etc);
        let passwd = etc.join("passwd");
        match fs::read_to_string(&passwd) {
            Ok(existing) => {
                let mut updated = existing;
                let mut changed = false;
                for (prefix, line) in [
                    (
                        "pulse:",
                        "pulse:x:51:51:PulseAudio:/var/run/pulse:/bin/false\n",
                    ),
                    // OpenNTPD's privilege-separation user (apk --no-scripts
                    // never runs the package's pre-install that creates it).
                    (
                        "_ntp:",
                        "_ntp:x:123:123:OpenNTPD:/var/empty:/sbin/nologin\n",
                    ),
                    // dbus-daemon's own user, same reason: its system.conf
                    // says `<user>messagebus</user>` and the daemon drops to
                    // it right after binding the listener, so without this
                    // line `--system` cannot look the name up and exits 1
                    // within tens of milliseconds -- which is exactly the
                    // restart loop the dbus-system service fell into. uid/gid
                    // 81 is what Alpine's dbus pre-install reserves, so the
                    // file ownerships apk laid down line up. The session bus
                    // never needed it: session.conf names no user.
                    (
                        "messagebus:",
                        "messagebus:x:81:81:dbus:/dev/null:/sbin/nologin\n",
                    ),
                ] {
                    if !updated.lines().any(|l| l.starts_with(prefix)) {
                        if !updated.ends_with('\n') {
                            updated.push('\n');
                        }
                        updated.push_str(line);
                        changed = true;
                    }
                }
                if changed {
                    fs::write(&passwd, updated).unwrap();
                }
            }
            Err(_) => {
                fs::write(
                    &passwd,
                    "root:x:0:0:root:/root:/bin/sh\n\
                     pulse:x:51:51:PulseAudio:/var/run/pulse:/bin/false\n\
                     _ntp:x:123:123:OpenNTPD:/var/empty:/sbin/nologin\n\
                     messagebus:x:81:81:dbus:/dev/null:/sbin/nologin\n\
                     nobody:x:65534:65534:nobody:/:/bin/false\n",
                )
                .unwrap();
            }
        }
        let group = etc.join("group");
        let extras = [
            ("pulse:", "pulse:x:51:\n"),
            ("pulse-access:", "pulse-access:x:52:root\n"),
            ("audio:", "audio:x:29:root,pulse\n"),
            ("_ntp:", "_ntp:x:123:\n"),
            ("messagebus:", "messagebus:x:81:\n"),
        ];
        match fs::read_to_string(&group) {
            Ok(existing) => {
                let mut updated = existing;
                let mut changed = false;
                for (prefix, line) in extras {
                    if !updated.lines().any(|l| l.starts_with(prefix)) {
                        if !updated.ends_with('\n') {
                            updated.push('\n');
                        }
                        updated.push_str(line);
                        changed = true;
                    }
                }
                if changed {
                    fs::write(&group, updated).unwrap();
                }
            }
            Err(_) => {
                fs::write(
                    &group,
                    "root:x:0:root\n\
                     audio:x:29:root,pulse\n\
                     pulse:x:51:\n\
                     pulse-access:x:52:root\n\
                     _ntp:x:123:\n\
                     nobody:x:65534:\n",
                )
                .unwrap();
            }
        }
        let home = rootfs.join("var/run/pulse");
        let _ = fs::create_dir_all(&home);
        let _ = fs::create_dir_all(rootfs.join("var/lib/pulse"));
        // OpenNTPD's chroot directory (must exist, empty, root-owned).
        let _ = fs::create_dir_all(rootfs.join("var/empty"));
        // Ollama model + temp dirs on the disk-backed tree (never /tmp).
        // Profile/labwc/init export OLLAMA_MODELS/TMPDIR here; mkdir at image
        // build so a fresh install does not race the first pull.
        let _ = fs::create_dir_all(rootfs.join("home/moebius/.ollama/models"));
        let _ = fs::create_dir_all(rootfs.join("home/moebius/.ollama/tmp"));
        let _ = fs::create_dir_all(rootfs.join("var/cache/ollama/models"));
        let _ = fs::create_dir_all(rootfs.join("var/cache/ollama/tmp"));
    }

    /// Cross-compile wavplay (`tools/wavplay`, Rust) as a static musl binary
    /// and return its path. Best-effort: without it the audio driver simply
    /// has no bundled test client.
    fn wavplay(&self) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("wavplay");
        let triple = self.musl_rust_triple();
        let executable = dir
            .join("target")
            .join(triple)
            .join("release")
            .join("wavplay");
        let newest_src = ["src/main.rs", "Cargo.toml"]
            .iter()
            .filter_map(|rel| fs::metadata(dir.join(rel)).ok()?.modified().ok())
            .max();
        if let (Ok(bin_meta), Some(src_mtime)) = (fs::metadata(&executable), newest_src) {
            if let Ok(bin_mtime) = bin_meta.modified() {
                if bin_mtime >= src_mtime && self.cached_for_target(&executable) {
                    return executable;
                }
            }
        }

        println!("Compiling wavplay (Rust, {triple})...");
        let _ = Ext::new("rustup")
            .arg("target")
            .arg("add")
            .arg(triple)
            .status();
        let mut cargo = Ext::new("cargo");
        cargo
            .current_dir(&dir)
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg(triple);
        // Static non-PIE, plus the cross linker when the host's `cc` cannot
        // link this target at all: see `cross_build_env`.
        for (k, v) in self.cross_build_env() {
            cargo.env(k, v);
        }
        let status = cargo.status();
        if !status.success() {
            eprintln!("warning: wavplay build failed; /dev/dsp has no test client");
        }
        executable
    }

    /// Install the Eclipse-native init as the default PID 1: copy the binary to
    /// `/sbin/eclipse-init`, repoint `/sbin/init` at it (overriding the busybox
    /// fallback laid down by `install_busybox_init`), and seed
    /// `/etc/eclipse/services/` with a documented example. Returns `true` on
    /// success, `false` (leaving busybox init as PID 1) if the build is absent.
    fn install_eclipse_init(&self, rootfs: &Path, musl: &Path) -> bool {
        let bin = self.eclipse_init(musl);
        if !bin.is_file() {
            eprintln!("warning: eclipse-init not built; keeping busybox init as PID 1");
            return false;
        }
        let sbin = rootfs.join("sbin");
        let _ = fs::create_dir_all(&sbin);
        let dst = sbin.join("eclipse-init");
        let _ = fs::remove_file(&dst);
        fs::copy(&bin, &dst).unwrap();

        // /sbin/init -> eclipse-init (the kernel boots INIT=/sbin/init).
        let init_link = sbin.join("init");
        let _ = fs::remove_file(&init_link);
        #[cfg(unix)]
        {
            let _ = unix::fs::symlink("eclipse-init", &init_link);
        }

        // Service directory + a documented (inert) example, plus the default
        // boot services: DHCP, the seat manager and the labwc session.
        let svc_dir = rootfs.join("etc").join("eclipse").join("services");
        let _ = fs::create_dir_all(&svc_dir);
        Self::write_init_services(&svc_dir, self.1.has_desktop());

        // The startup chime's asset, which a oneshot below plays.
        let share = rootfs.join("usr").join("share").join("eclipse");
        let _ = fs::create_dir_all(&share);
        let mp3_src = PROJECT_DIR
            .join("assets")
            .join("audio")
            .join("Eclipse_Awakening.mp3");
        if mp3_src.is_file() {
            let _ = fs::copy(&mp3_src, share.join("Eclipse_Awakening.mp3"));
        } else {
            eprintln!("warning: assets/audio/Eclipse_Awakening.mp3 missing; boot sound disabled");
        }

        // The two init wrappers (the labwc one is written by desktop.rs).
        let localbin = rootfs.join("usr").join("local").join("bin");
        let _ = fs::create_dir_all(&localbin);
        Self::write_init_wrappers(&localbin, &svc_dir);

        // Default desktop selector: `labwc` on the desktop variant (the
        // hardware default), `none` on the minimal one. A boot with
        // `desktop=xorg` on the kernel cmdline (see `make qemu`) overrides this;
        // editing this file changes the default persistently. See
        // eclipse-init's `selected_desktop`.
        //
        // This is the ONLY writer of the file, on purpose. It hardcoded `labwc`
        // while the minimal variant wrote `none` from `install_desktop_stack` --
        // which runs BEFORE this function on the from-scratch path and AFTER it
        // on the incremental one. So `cargo rootfs --variant minimal` left
        // `labwc` behind and only a following `cargo image` corrected it: a
        // minimal rootfs that boots hunting for a compositor it does not carry,
        // depending on which of the two commands ran last.
        Self::write_desktop_session(rootfs, self.1.default_session());

        println!(
            "Installed eclipse-init as PID 1 with udhcpc, dbus, seatd, labwc, xorg{} and \
             boot-sound services.",
            if self.1.has_desktop() {
                ", pulseaudio"
            } else {
                " (no pulseaudio: the minimal variant installs no apk closure)"
            }
        );
        true
    }

    /// The service files `install_eclipse_init` lays down in
    /// `/etc/eclipse/services`.
    ///
    /// Its own function because the caller cross-compiles a musl binary before
    /// it gets this far and returns early when that build did not land, so
    /// nothing can ask the table anything through it. And a table is exactly
    /// what it is: every `after =` has to name a service that exists, a
    /// `wait_socket =` has to be ordered after whatever binds that socket,
    /// every key has to be one `tools/eclipse-init` parses -- an unknown key is
    /// a gate that silently does not exist -- and the `desktop =` column is
    /// what decides which of the two sessions each service belongs to.
    ///
    /// `has_desktop` is the variant: `desktop =` is the column that keeps a
    /// session's services out of a minimal image, and it only works for
    /// services that belong to a session. A service whose binary comes from the
    /// apk closure but is session-agnostic -- `pulseaudio` is the one -- has no
    /// value to put in that column and would be written into an image that
    /// installs no apk closure at all, where `eclipse-init` then burns its 20
    /// starts and 162 seconds of console on a binary that is not there. So the
    /// variant is a parameter, and the table itself is what leaves such a row
    /// out.
    pub(super) fn write_init_services(svc_dir: &Path, has_desktop: bool) {
        fs::write(
            svc_dir.join("example.service.txt"),
            b"# Eclipse init service file. Copy to '<name>.service' to enable.\n\
              #\n\
              # exec  = /usr/sbin/mydaemon --foreground   (required; argv, space-split)\n\
              # type  = respawn                            (respawn | oneshot; default oneshot)\n\
              # after = othersvc                           (optional; space-separated deps)\n\
              # requires = othersvc                        (optional; deps this service\n\
              #                                             CANNOT work without -- if one\n\
              #                                             is given up on for the boot, so\n\
              #                                             is this service. Implies after)\n\
              # timeout = 90                               (optional; seconds a 'oneshot'\n\
              #                                             may run before init stops\n\
              #                                             waiting and kills it. 0 or\n\
              #                                             'none' waits for ever;\n\
              #                                             default 90)\n\
              # cmdline = dbus.selftest                     (optional; start only when\n\
              #                                              this token is on the\n\
              #                                              kernel command line)\n\
              # desktop = labwc                             (optional; start only under\n\
              #                                             this session: labwc | xorg)\n\
              # log = /tmp/mysvc.log                        (optional; where the service's\n\
              #                                             output goes, instead of\n\
              #                                             /dev/null)\n\
              # wait_socket = /run/other.sock              (optional; block each start,\n\
              #                                             bounded, until this unix\n\
              #                                             socket exists)\n\
              # wait_path = /dev/input/event0              (optional; block each start,\n\
              #                                             bounded, until this path\n\
              #                                             exists -- any file type)\n\
              #\n\
              # 'oneshot' runs to completion in order during boot; 'respawn' is\n\
              # supervised and restarted if it exits. No shell is involved.\n",
        )
        .unwrap();

        // udhcpc: the kernel brings the link up but sets no address, so DHCP is
        // what gives the machine an IP and default route at boot -- before, it
        // only happened when the desktop's prepare step ran udhcpc by hand.
        // Supervised: the foreground client keeps renewing the lease, and init
        // restarts it (with backoff) if it dies.
        fs::write(
            svc_dir.join("udhcpc.service"),
            b"# DHCP client (foreground). See /usr/local/bin/eclipse-udhcpc.\n\
              exec = /usr/local/bin/eclipse-udhcpc\n\
              type = respawn\n",
        )
        .unwrap();

        // NTP: after DHCP so pool.ntp.org resolves. Foreground wrapper.
        fs::write(
            svc_dir.join("ntpd.service"),
            b"# NTP client (foreground). See /usr/local/bin/eclipse-ntpd.\n\
              exec = /usr/local/bin/eclipse-ntpd\n\
              type = respawn\n\
              after = udhcpc\n\
              log = /tmp/ntpd.log\n",
        )
        .unwrap();

        // seatd: the seat manager wlroots/labwc open DRM and input devices
        // through. Started before labwc so its socket exists when the
        // compositor connects (init's `after` orders the start; the labwc
        // wrapper also falls back to libseat's builtin backend if it is not up
        // yet, so a start-order race just costs one backoff retry).
        fs::write(
            svc_dir.join("seatd.service"),
            b"# Seat manager (foreground). See /usr/local/bin/eclipse-seatd.\n\
              # labwc-only: seatd/libseat is the Wayland seat path; Xorg on the\n\
              # framebuffer opens its devices directly and does not need it.\n\
              exec = /usr/local/bin/eclipse-seatd\n\
              type = respawn\n\
              desktop = labwc\n",
        )
        .unwrap();

        // D-Bus session bus. NOT desktop-restricted: it is the first thing
        // SDL_Init() looks for (SDL_DBus_Init), the thing GtkApplication exits
        // without, and the thing every "am I already running?" check is built
        // on, so an Xorg session and a labwc session both want it. Ordered
        // ahead of the sessions but with no `wait_socket =`: a bus that is not
        // there must cost the boot nothing, and no service started at boot
        // talks to it -- the clients that do (SDL games, GTK apps) start
        // minutes later, long after the daemon has bound its socket.
        fs::write(
            svc_dir.join("dbus.service"),
            b"# D-Bus session bus. See /usr/local/bin/eclipse-dbus.\n\
              # Address: unix:path=/run/user/0/bus, the same one eclipse-init,\n\
              # /etc/profile, the labwc environment and the SDL wrapper all\n\
              # export as DBUS_SESSION_BUS_ADDRESS.\n\
              exec = /usr/local/bin/eclipse-dbus\n\
              type = respawn\n\
              log = /tmp/dbus.log\n",
        )
        .unwrap();

        portal::write_service(svc_dir);

        // D-Bus system bus, on libdbus's compiled-in default address. A second
        // bus, independent of the session one above; see eclipse-dbus-system
        // for why it cannot be the same socket.
        fs::write(
            svc_dir.join("dbus-system.service"),
            b"# D-Bus system bus. See /usr/local/bin/eclipse-dbus-system.\n\
              # Address: unix:path=/run/dbus/system_bus_socket, which libdbus\n\
              # uses with no environment variable set -- which is how\n\
              # pulseaudio --system looks for it.\n\
              exec = /usr/local/bin/eclipse-dbus-system\n\
              type = respawn\n\
              log = /tmp/dbus-system.log\n",
        )
        .unwrap();

        // Session-bus probe, opt-in. `cmdline = dbus.selftest` keeps it out of
        // a normal boot entirely; a boot with `dbus.selftest` on the kernel
        // command line runs the same checks the desktop menu offers and prints
        // DBUSPROBE: PASS/FAIL to the console. It is the only way to verify
        // the bus (and the unix-socket/SO_PEERCRED/poll paths under it) on a
        // machine with no desktop installed -- QEMU, for instance.
        fs::write(
            svc_dir.join("dbus-selftest.service"),
            b"# Session-bus probe. Boot with `dbus.selftest` on the cmdline.\n\
              exec = /bin/eclipse-dbusd --selftest\n\
              type = oneshot\n\
              after = dbus\n\
              requires = dbus\n\
              cmdline = dbus.selftest\n\
              wait_socket = /run/user/0/bus\n\
              log = /dev/console\n",
        )
        .unwrap();

        // GTK caches (gschemas.compiled, pixbuf loaders.cache) before the
        // first GTK client. A oneshot runs to completion before anything
        // ordered after it forks, so labwc -- and every client it launches --
        // sees them. See desktop::write_gtk_caches_wrapper for why Firefox's
        // chrome text depends on this.
        fs::write(
            svc_dir.join("gtk-caches.service"),
            b"# GTK caches for the Wayland session. See /usr/local/bin/eclipse-gtk-caches.\n\
              exec = /usr/local/bin/eclipse-gtk-caches\n\
              type = oneshot\n\
              desktop = labwc\n",
        )
        .unwrap();

        // labwc: launch the hardened wrapper so init, shells and login sessions all
        // take the same renderer/env path. Wait for seatd + settled /dev/input
        // before start — without udevd libinput scans input nodes exactly once.
        fs::write(
            svc_dir.join("labwc.service"),
            b"# labwc Wayland session (wrapper; env from eclipse-init + wrapper fallback).\n\
              # wait_socket: seatd readiness (after= only orders the fork).\n\
              # wait_path: /dev/input must be non-empty and settled -> no udev\n\
              # hotplug, so starting between keyboard and mouse enumeration\n\
              # leaves the late device dead for the whole session.\n\
              # gtk-caches: oneshot, so it has COMPLETED before this forks.\n\
              exec = /usr/local/bin/labwc\n\
              type = respawn\n\
              after = seatd gtk-caches dbus\n\
              requires = seatd\n\
              wait_socket = /run/seatd.sock\n\
              wait_path = /dev/input\n\
              desktop = labwc\n\
              log = /tmp/labwc.log\n",
        )
        .unwrap();

        // Desktop clients: started by init, NOT by labwc's autostart.
        // labwc double-forks `sh ~/.config/labwc/autostart` and that ash
        // SIGSEGVs on this kernel (musl mallocng, NULL context). Single-
        // threaded eclipse-init forking these wrappers is fine.
        // wait_socket on both clients: init parks (10 ms stat poll, no forks)
        // until labwc's socket is up, instead of each wrapper forking a
        // busybox `sleep 0.1` per iteration — ~40-80 spawns per boot exactly
        // while the compositor was demand-paging itself. init clears stale
        // /run at boot, so a fresh labwc always binds wayland-0.
        fs::write(
            svc_dir.join("lunarbg.service"),
            b"# Procedural wallpaper (wlr-layer-shell). See eclipse-lunarbg.\n\
              exec = /usr/local/bin/eclipse-lunarbg\n\
              type = respawn\n\
              after = labwc\n\
              requires = labwc\n\
              wait_socket = /run/user/0/wayland-0\n\
              desktop = labwc\n",
        )
        .unwrap();
        fs::write(
            svc_dir.join("lunarbar.service"),
            b"# Two-bar panel (wlr-layer-shell). See eclipse-lunarbar.\n\
              exec = /usr/local/bin/eclipse-lunarbar\n\
              type = respawn\n\
              after = labwc\n\
              requires = labwc\n\
              wait_socket = /run/user/0/wayland-0\n\
              desktop = labwc\n",
        )
        .unwrap();

        // The clipboard manager. `respawn` like the other two clients: it has
        // to be alive for the whole session, because on Wayland the selection
        // belongs to the process offering it — a daemon that died takes the
        // clipboard with it the moment the application that copied exits.
        fs::write(
            svc_dir.join("lunarclip.service"),
            b"# Clipboard manager + history (wlr-data-control). See eclipse-lunarclip.\n\
              exec = /usr/local/bin/eclipse-lunarclip\n\
              type = respawn\n\
              after = labwc\n\
              requires = labwc\n\
              wait_socket = /run/user/0/wayland-0\n\
              desktop = labwc\n",
        )
        .unwrap();

        // Load the X keymap into Xwayland once the compositor is up. Wayland
        // clients compile their own keymap; the X server does not, so without
        // this X11 clients (xterm) type nothing. `oneshot`: apply once and
        // exit. The wrapper waits for the /tmp/.X11-unix/X* socket itself, so
        // `after = labwc` (fork order) + the wayland `wait_socket` gate is
        // enough. See desktop::write_xkbmap_wrapper.
        fs::write(
            svc_dir.join("xkbmap.service"),
            b"# X keymap for Xwayland (X11 clients get none otherwise). See eclipse-xkbmap.\n\
              exec = /usr/local/bin/eclipse-xkbmap\n\
              type = oneshot\n\
              after = labwc\n\
              requires = labwc\n\
              wait_socket = /run/user/0/wayland-0\n\
              desktop = labwc\n",
        )
        .unwrap();

        // Xorg session (the framebuffer/fbdev X stack). Selected instead of
        // labwc when the boot picks `desktop=xorg` (e.g. `make qemu`). Runs the
        // eclipse-xorg wrapper, which starts X + the .xinitrc session on VT.
        fs::write(
            svc_dir.join("xorg.service"),
            b"# Xorg session (fbdev on /dev/fb0). See /usr/local/bin/eclipse-xorg.\n\
              exec = /usr/local/bin/eclipse-xorg\n\
              type = respawn\n\
              after = dbus\n\
              desktop = xorg\n",
        )
        .unwrap();

        // PulseAudio: system instance over ALSA hw:0,0. Session-agnostic so
        // both labwc and Xorg get a mixer.
        //
        // Do NOT `wait_path = /dev/snd/pcmC0D0p`: start order is alphabetical,
        // so that wait sat ahead of seatd/labwc and burned a full 8 s on every
        // machine without HDA (VirtualBox AC97 has no driver here; the node
        // never appears). Pulse loads the ALSA sink after its socket, and
        // `eclipse-boot-sound` already polls for a PCM; a missing card just
        // means no sink, not a delayed desktop.
        //
        // Session-agnostic is exactly why this row needs the variable above
        // instead of a `desktop =` line: `pulseaudio` belongs to neither
        // session, so no value in that column describes it, and the binary it
        // execs comes from the apk closure that `install_desktop_stack`
        // installs only for `desktop`. Written into a minimal image it is a row
        // that can never start -- `eclipse-pulseaudio: pulseaudio not
        // installed`, 20 times, 8 seconds apart, 162 seconds of console before
        // `CRASH_START_LIMIT` gives up on it. The two services that name it
        // (`boot-sound`, `boot-sound-xorg`) are both `desktop =`-guarded, so
        // leaving it out here strands no `requires =`.
        if has_desktop {
            fs::write(
                svc_dir.join("pulseaudio.service"),
                b"# PulseAudio sound server (system instance). See eclipse-pulseaudio.\n\
                  exec = /usr/local/bin/eclipse-pulseaudio\n\
                  type = respawn\n\
                  after = dbus-system\n\
                  log = /tmp/pulseaudio.log\n",
            )
            .unwrap();
        }

        // Boot chime: play once after the session is up so NVIDIA HDMI audio
        // (ELD/SET_HDMI_ENABLE) has had a DRM query. Oneshot wrapper forks
        // mpg123 and exits so init is not blocked for the length of the track.
        fs::write(
            svc_dir.join("boot-sound.service"),
            b"# Startup MP3 (Eclipse_Awakening). See /usr/local/bin/eclipse-boot-sound.\n\
              exec = /usr/local/bin/eclipse-boot-sound\n\
              type = oneshot\n\
              after = pulseaudio lunarbar\n\
              requires = pulseaudio lunarbar\n\
              wait_socket = /run/user/0/wayland-0\n\
              desktop = labwc\n\
              log = /tmp/boot-sound.log\n\
              timeout = 15\n",
        )
        .unwrap();
        fs::write(
            svc_dir.join("boot-sound-xorg.service"),
            b"# Startup MP3 under the Xorg session.\n\
              exec = /usr/local/bin/eclipse-boot-sound\n\
              type = oneshot\n\
              after = pulseaudio xorg\n\
              requires = pulseaudio xorg\n\
              desktop = xorg\n\
              log = /tmp/boot-sound.log\n\
              timeout = 15\n",
        )
        .unwrap();
    }

    /// The wrapper scripts `install_eclipse_init` lays down in
    /// `/usr/local/bin`, plus the pass that makes every one of them
    /// executable.
    ///
    /// Its own function for the same reason as [`Self::write_init_services`],
    /// and what there is to ask of it is the other half of that table: every
    /// `exec =` in it names a script something really writes, and no wrapper
    /// ends up without its x bit -- `eclipse-oopslog` shipped 0644 once,
    /// `execve` answered EACCES, and init respawned it for the whole boot.
    /// Takes `svc_dir` because `write_oopslog` adds a service of its own.
    pub(super) fn write_init_wrappers(localbin: &Path, svc_dir: &Path) {
        fs::write(
            localbin.join("eclipse-boot-sound"),
            b"#!/bin/sh\n\
              # Detach so the oneshot does not hold up lunarbar/xkbmap.\n\
              setsid /usr/local/bin/eclipse-boot-sound-play </dev/null >>/tmp/boot-sound.log 2>&1 &\n\
              exit 0\n",
        )
        .unwrap();
        fs::write(
            localbin.join("eclipse-boot-sound-play"),
            b"#!/bin/sh\n\
              # Wait for PulseAudio (or a raw PCM), unmute, play the startup MP3 once.\n\
              MP3=/usr/share/eclipse/Eclipse_Awakening.mp3\n\
              [ -f \"$MP3\" ] || { echo \"eclipse-boot-sound: missing $MP3\" >&2; exit 0; }\n\
              # Every mpg123 here runs under a watchdog. A player wedged in the\n\
              # audio path never comes back on its own, and it holds the PCM for\n\
              # the whole session (the device is exclusive: every later open\n\
              # answers EBUSY), so the chime would cost all later sound. The\n\
              # track is a few seconds long; anything past MAXPLAY is stuck.\n\
              MAXPLAY=20\n\
              # Prefer the real binary. Wrappers like mpg123-pulse already pass\n\
              # `-o pulse`; if we then add `-o alsa` the later flag wins, mpg123\n\
              # grabs the exclusive PCM, hangs, and the chime (and session audio)\n\
              # loops until the watchdog -- the \"final loop\" seen on Ubuntu.\n\
              MPG123=\n\
              for c in mpg123.bin mpg123; do\n\
              \x20 command -v \"$c\" >/dev/null 2>&1 && { MPG123=$c; break; }\n\
              done\n\
              play() {\n\
              \x20 \"$MPG123\" \"$@\" &\n\
              \x20 p=$!\n\
              \x20 ( i=0\n\
              \x20\x20 while [ \"$i\" -lt \"$MAXPLAY\" ]; do\n\
              \x20\x20\x20 kill -0 \"$p\" 2>/dev/null || exit 0\n\
              \x20\x20\x20 i=$((i + 1))\n\
              \x20\x20\x20 sleep 1\n\
              \x20\x20 done\n\
              \x20\x20 echo \"eclipse-boot-sound: $MPG123 stuck after ${MAXPLAY}s; killing it\" >&2\n\
              \x20\x20 kill -TERM \"$p\" 2>/dev/null\n\
              \x20\x20 sleep 1\n\
              \x20\x20 kill -KILL \"$p\" 2>/dev/null ) &\n\
              \x20 w=$!\n\
              \x20 wait \"$p\"\n\
              \x20 r=$?\n\
              \x20 kill \"$w\" 2>/dev/null\n\
              \x20 wait \"$w\" 2>/dev/null\n\
              \x20 return \"$r\"\n\
              }\n\
              i=0\n\
              while [ \"$i\" -lt 20 ]; do\n\
              \x20 if [ -S /run/pulse/native ] || [ -e /dev/snd/pcmC0D0p ] || [ -e /dev/dsp ]; then\n\
              \x20\x20 break\n\
              \x20 fi\n\
              \x20 i=$((i + 1))\n\
              \x20 sleep 1\n\
              done\n\
              if [ ! -S /run/pulse/native ] && [ ! -e /dev/snd/pcmC0D0p ] && [ ! -e /dev/dsp ]; then\n\
              \x20 echo 'eclipse-boot-sound: no PCM device' >&2\n\
              \x20 exit 0\n\
              fi\n\
              # Compositor DRM query enables NVIDIA HDMI packets; give it a beat.\n\
              sleep 2\n\
              if command -v pactl >/dev/null 2>&1 && [ -S /run/pulse/native ]; then\n\
              \x20 pactl set-sink-volume @DEFAULT_SINK@ 70% 2>/dev/null\n\
              \x20 pactl set-sink-mute @DEFAULT_SINK@ 0 2>/dev/null\n\
              \x20 # Force every sink OUT of SUSPENDED via the EXPLICIT resume path.\n\
              \x20 # The kernel PCM's implicit cold resume (run from the sink IO thread\n\
              \x20 # when a stream attaches) is unreliable under SMP+KVM: it plays silent\n\
              \x20 # or hangs. The explicit `suspend-sink 0` resume always works, so do it\n\
              \x20 # once now; with no module-suspend-on-idle loaded the sink then stays\n\
              \x20 # active for the whole session and every later play (mpg123/paplay/\n\
              \x20 # Firefox) hits an already-running PCM instead of the broken resume.\n\
              \x20 for s in $(pactl list short sinks 2>/dev/null | awk '{print $1}'); do\n\
              \x20\x20 pactl suspend-sink \"$s\" 0 2>/dev/null || true\n\
              \x20 done\n\
              elif command -v amixer >/dev/null 2>&1; then\n\
              \x20 amixer -q set Master 70% unmute 2>/dev/null\n\
              fi\n\
              if [ -n \"$MPG123\" ]; then\n\
              \x20 if [ -S /run/pulse/native ]; then\n\
              \x20\x20 # Pulse is up: ONLY use the pulse output. Opening ALSA here\n\
              \x20\x20 # races Pulse for the exclusive PCM and wedges the player.\n\
              \x20\x20 play -q -o pulse --encoding s16 --no-gapless \"$MP3\"\n\
              \x20\x20 # drain can block forever if the sink is wedged; bound it.\n\
              \x20\x20 if command -v pactl >/dev/null 2>&1; then\n\
              \x20\x20\x20 pactl drain 2>/dev/null &\n\
              \x20\x20\x20 d=$!\n\
              \x20\x20\x20 ( sleep 5; kill \"$d\" 2>/dev/null ) &\n\
              \x20\x20\x20 wait \"$d\" 2>/dev/null || true\n\
              \x20\x20 fi\n\
              \x20 else\n\
              \x20\x20 play -q -o alsa,oss --encoding s16 --no-gapless \"$MP3\"\n\
              \x20 fi\n\
              \x20 exit 0\n\
              fi\n\
              echo 'eclipse-boot-sound: mpg123 not installed' >&2\n\
              exit 0\n",
        )
        .unwrap();
        fs::write(
            localbin.join("eclipse-pulseaudio"),
            b"#!/bin/sh\n\
              # Eclipse OS: PulseAudio system instance. User-mode PulseAudio\n\
              # refuses uid 0; --system + the `pulse` account (ensure_pulse_accounts)\n\
              # is the path that actually starts. Foreground: init supervises us.\n\
              command -v pulseaudio >/dev/null 2>&1 || {\n\
              \x20 echo 'eclipse-pulseaudio: pulseaudio not installed' >&2\n\
              \x20 sleep 8\n\
              \x20 exit 127\n\
              }\n\
              if ! grep -q '^pulse:' /etc/passwd 2>/dev/null; then\n\
              \x20 echo 'eclipse-pulseaudio: no pulse user in /etc/passwd' >&2\n\
              \x20 sleep 8\n\
              \x20 exit 1\n\
              fi\n\
              mkdir -p /run/pulse /run/user/0/pulse /var/lib/pulse /var/run/pulse\n\
              chmod 0755 /run/pulse /var/run/pulse 2>/dev/null || true\n\
              chmod 0700 /var/lib/pulse /run/user/0 /run/user/0/pulse 2>/dev/null || true\n\
              chown pulse:pulse /run/pulse /var/run/pulse /var/lib/pulse 2>/dev/null || true\n\
              # init hands every service XDG_RUNTIME_DIR=/run/user/0 (uid 0).\n\
              # After --system drops to pulse, pa_get_runtime_dir would reject\n\
              # that directory as 'not owned by us' and exit.\n\
              unset XDG_RUNTIME_DIR\n\
              # Same for XDG_CONFIG_HOME=/root/.config: after the drop to `pulse`\n\
              # nothing under /root is reachable; let config/cookie paths derive\n\
              # from HOME below (writable by pulse).\n\
              unset XDG_CONFIG_HOME\n\
              export HOME=/var/run/pulse\n\
              export PULSE_RUNTIME_PATH=/run/pulse\n\
              export PULSE_STATE_PATH=/var/lib/pulse\n\
              # Headroom for shared libs + /etc/group during initgroups(3).\n\
              # Without it a fd table inherited near RLIMIT_NOFILE makes Pulse\n\
              # die with: Failed to change group list: No file descriptors available\n\
              ulimit -n 65536 2>/dev/null || ulimit -n 4096 2>/dev/null || true\n\
              # --log-level=info: the sink's 'Trying resume...', 'Resumed successfully...' and\n\
              # 'Starting playback.' are info-level; audio-probe [pulse-play] reads them from the log.\n\
              # A system.pa whose module-native-protocol-unix has no\n\
              # auth-cookie-enabled=0 CANNOT bind /run/pulse/native: the module\n\
              # loads-or-creates a cookie under a path `pulse` cannot write and\n\
              # fails to initialise, leaving a daemon that is alive, owns the\n\
              # cards and listens nowhere (every client: Connection refused).\n\
              # xtask keeps its own copy of the working script next to it; use\n\
              # that one rather than start a daemon nobody can reach.\n\
              PA_SCRIPT=\n\
              if ! grep -q 'auth-cookie-enabled=0' /etc/pulse/system.pa 2>/dev/null \\\n\
              \x20\x20 && [ -r /etc/pulse/system.pa.eclipse ]; then\n\
              \x20 echo 'eclipse-pulseaudio: /etc/pulse/system.pa cannot bind the socket; using /etc/pulse/system.pa.eclipse' >&2\n\
              \x20 PA_SCRIPT='-n --file=/etc/pulse/system.pa.eclipse'\n\
              fi\n\
              # shellcheck disable=SC2086 -- PA_SCRIPT is two words or none.\n\
              exec pulseaudio --system --disallow-exit --exit-idle-time=-1 --daemonize=no --use-pid-file=no --realtime=false --log-target=stderr --log-level=info $PA_SCRIPT\n",
        )
        .unwrap();

        fs::write(
            localbin.join("eclipse-udhcpc"),
            b"#!/bin/sh\n\
              # Eclipse OS: foreground DHCP on the first real interface, for\n\
              # eclipse-init. Runs udhcpc in the foreground (init supervises it)\n\
              # and keeps renewing the lease; if it dies, init respawns it.\n\
              command -v udhcpc >/dev/null 2>&1 || { echo 'eclipse-udhcpc: udhcpc not found' >&2; sleep 5; exit 127; }\n\
              # -s is NOT optional: busybox udhcpc's compiled-in default script\n\
              # path is not where Eclipse stages the script, so without -s the\n\
              # lease is obtained but never APPLIED -- the address, resolv.conf\n\
              # and route are all set by this script on the 'bound' event.\n\
              SCRIPTv4=/usr/share/udhcpc/default.script\n\
              [ -x \"$SCRIPTv4\" ] || SCRIPTv4=/etc/udhcpc/default.script\n\
              # (udhcpc6 would need its own service: anything after an `exec`\n\
              # that succeeds is unreachable, so only IPv4 DHCP runs here.)\n\
              for i in $(ip -o link show 2>/dev/null | sed 's/^[0-9]*: //; s/[@:].*//' | grep -v '^lo'); do\n\
              \x20 exec udhcpc -i \"$i\" -f -R -s \"$SCRIPTv4\"\n\
              done\n\
              # `ip` may not have listed anything yet (netlink dump raced the\n\
              # NIC probe). eth0 is the sequential name the kernel assigns to\n\
              # the first Ethernet NIC.\n\
              echo 'eclipse-udhcpc: no interface from ip link; trying eth0' >&2\n\
              exec udhcpc -i eth0 -f -R -s \"$SCRIPTv4\"\n",
        )
        .unwrap();
        Self::write_oopslog(localbin, svc_dir);
        Self::write_hunterd(localbin, svc_dir);

        Self::write_dbus_wrapper(localbin);

        fs::write(
            localbin.join("eclipse-seatd"),
            b"#!/bin/sh\n\
              # Eclipse OS: run the seatd daemon in the foreground for\n\
              # eclipse-init. As root it needs no -u/-g; the socket lands at\n\
              # /run/seatd.sock, which the (root) compositor can always open.\n\
              # Capture seatd's own log (init wires a service's stdio to\n\
              # /dev/null, same reason the labwc wrapper captures its log):\n\
              # a compositor that hangs right after \"Loading user-specified\n\
              # backends\" is most likely blocked in the libseat handshake, and\n\
              # without this there is NO visibility into what seatd is doing\n\
              # (or not doing) about that connection.\n\
              LOG=/tmp/seatd.log\n\
              : > \"$LOG\" 2>/dev/null || true\n\
              for d in /usr/bin /bin /usr/sbin /sbin; do\n\
              \x20 [ -x \"$d/seatd\" ] && exec \"$d/seatd\" -l info >>\"$LOG\" 2>&1\n\
              done\n\
              # NOT INSTALLED. Say so where it can actually be found: init wires\n\
              # a service's stdio to /dev/null, so a bare `>&2` here vanished and\n\
              # left an EMPTY /tmp/seatd.log next to an endless respawn storm --\n\
              # the single most confusing symptom this image can produce, and one\n\
              # that cost a full debugging session to trace back to a missing\n\
              # package. Write the reason INTO the log and onto the console.\n\
              MSG='eclipse-seatd: seatd is NOT INSTALLED -- the whole Wayland\n\
              session (labwc, lunarbg, lunarbar) cannot start without it. Fix:\n\
              apk add seatd labwc  (needs network at image-build time).'\n\
              echo \"$MSG\" >>\"$LOG\" 2>/dev/null || true\n\
              echo \"$MSG\" > /dev/console 2>/dev/null || true\n\
              echo \"$MSG\" >&2\n\
              # Back off HARD rather than every 5 s: a missing package is not\n\
              # going to appear on its own, and the tight respawn loop it caused\n\
              # (seatd 100x, labwc 67x per run) is pure fork/exec churn that\n\
              # buys nothing and stresses the kernel for no reason.\n\
              sleep 60\n\
              exit 127\n",
        )
        .unwrap();

        // Locate labwc's Wayland socket, then exec the native client. Shared
        // by wallpaper + panel. The BOOT-path wait lives in eclipse-init now
        // (`wait_socket = /run/user/0/wayland-0` in the service files: a
        // native 10 ms stat poll, zero forks), so on boot the glob below hits
        // on the FIRST pass. The short loop only covers manual launches that
        // race a just-started compositor — `sleep 1`, not `sleep 0.1`: every
        // iteration forks a busybox, and 10x fewer forks matters more than
        // sub-second latency on a path init already made cold.
        let wait_wayland = "\
              : \"${XDG_RUNTIME_DIR:=/run/user/0}\"; export XDG_RUNTIME_DIR\n\
              [ -d \"$XDG_RUNTIME_DIR\" ] || { mkdir -p \"$XDG_RUNTIME_DIR\" && chmod 0700 \"$XDG_RUNTIME_DIR\"; }\n\
              export PATH=/usr/local/bin:/bin:/sbin:/usr/bin:/usr/sbin\n\
              export LANG=\"${LANG:-es_ES.UTF-8}\"\n\
              export TZ=\"${TZ:-Europe/Madrid}\"\n\
              i=0\n\
              while [ \"$i\" -lt 15 ]; do\n\
              \x20 for s in \"$XDG_RUNTIME_DIR\"/wayland-[0-9]*; do\n\
              \x20   [ -S \"$s\" ] || continue\n\
              \x20   WAYLAND_DISPLAY=$(basename \"$s\"); export WAYLAND_DISPLAY\n\
              \x20   break 2\n\
              \x20 done\n\
              \x20 sleep 1\n\
              \x20 i=$((i+1))\n\
              done\n\
              if [ -z \"${WAYLAND_DISPLAY:-}\" ]; then\n\
              \x20 echo \"$0: no wayland socket under $XDG_RUNTIME_DIR yet\" >&2\n\
              \x20 # No compositor. If labwc is not even installed, this can never\n\
              \x20 # succeed, so back off hard instead of respawning every ~15 s:\n\
              \x20 # the client is healthy, its compositor is simply absent.\n\
              \x20 for d in /usr/bin /bin /usr/sbin /sbin; do\n\
              \x20 \x20 [ -x \"$d/labwc\" ] && { sleep 2; exit 1; }\n\
              \x20 done\n\
              \x20 echo \"$0: labwc is not installed either -- backing off\" >&2\n\
              \x20 sleep 60; exit 1\n\
              fi\n";

        fs::write(
            localbin.join("eclipse-lunarbg"),
            format!(
                "#!/bin/sh\n\
                 # Eclipse OS: wallpaper client for eclipse-init (not labwc autostart).\n\
                 LOG=/tmp/lunarbg.log\n\
                 exec >>\"$LOG\" 2>&1\n\
                 {wait}\
                 command -v lunarbg >/dev/null 2>&1 || {{ echo 'eclipse-lunarbg: lunarbg missing'; sleep 5; exit 127; }}\n\
                 echo \"[eclipse-lunarbg] WAYLAND_DISPLAY=$WAYLAND_DISPLAY\"\n\
                 # labwc writes LUNARBG_ASPECT into its environment file, but\n\
                 # this client is started by eclipse-init — not as a labwc\n\
                 # child — so re-export a default for panels without EDID mm.\n\
                 export LUNARBG_ASPECT=\"${{LUNARBG_ASPECT:-16:9}}\"\n\
                 exec lunarbg --fps \"${{LUNARBG_FPS:-8}}\"\n",
                wait = wait_wayland
            )
            .as_bytes(),
        )
        .unwrap();
        fs::write(
            localbin.join("eclipse-lunarbar"),
            format!(
                "#!/bin/sh\n\
                 # Eclipse OS: panel client for eclipse-init (not labwc autostart).\n\
                 LOG=/tmp/lunarbar.log\n\
                 exec >>\"$LOG\" 2>&1\n\
                 {wait}\
                 command -v lunarbar >/dev/null 2>&1 || {{ echo 'eclipse-lunarbar: lunarbar missing'; sleep 5; exit 127; }}\n\
                 echo \"[eclipse-lunarbar] WAYLAND_DISPLAY=$WAYLAND_DISPLAY\"\n\
                 exec lunarbar\n",
                wait = wait_wayland
            )
            .as_bytes(),
        )
        .unwrap();

        fs::write(
            localbin.join("eclipse-lunarclip"),
            format!(
                "#!/bin/sh\n\
                 # Eclipse OS: clipboard manager for eclipse-init.\n\
                 # Wayland has no clipboard of its own: the selection is the\n\
                 # client that copied, and it dies with it. This is what keeps\n\
                 # a copy alive after its window closes, and what feeds the\n\
                 # Super+V history.\n\
                 LOG=/tmp/lunarclip.log\n\
                 exec >>\"$LOG\" 2>&1\n\
                 {wait}\
                 command -v lunarclip >/dev/null 2>&1 || {{ echo 'eclipse-lunarclip: lunarclip missing'; sleep 5; exit 127; }}\n\
                 echo \"[eclipse-lunarclip] WAYLAND_DISPLAY=$WAYLAND_DISPLAY\"\n\
                 # --check first, into the same log: when the clipboard does\n\
                 # not work the first question is always whether the\n\
                 # compositor has wlr-data-control at all, and the answer\n\
                 # being in the log is the difference between reading one file\n\
                 # and reproducing the session.\n\
                 lunarclip --check || true\n\
                 exec lunarclip --watch\n",
                wait = wait_wayland
            )
            .as_bytes(),
        )
        .unwrap();

        fs::write(
            localbin.join("eclipse-xorg"),
            b"#!/bin/sh\n\
              # Eclipse OS: start the Xorg session (fbdev on /dev/fb0) for\n\
              # eclipse-init. startx reads /root/.xserverrc (which execs X with\n\
              # -ac) and /root/.xinitrc (the session: XFCE or a WM+terminal).\n\
              # It blocks until the session ends; init then respawns us.\n\
              export HOME=/root\n\
              : \"${XDG_RUNTIME_DIR:=/run/user/0}\"; export XDG_RUNTIME_DIR\n\
              [ -d \"$XDG_RUNTIME_DIR\" ] || { mkdir -p \"$XDG_RUNTIME_DIR\" && chmod 0700 \"$XDG_RUNTIME_DIR\"; }\n\
              if ! command -v startx >/dev/null 2>&1; then\n\
              \x20 echo 'eclipse-xorg: startx not found (apk add xinit xorg-server xf86-video-fbdev)' >&2\n\
              \x20 sleep 5; exit 127\n\
              fi\n\
              # NOTE: X is kept off DRM/card0 by the KERNEL, which does not create\n\
              # /dev/dri/card0 when the cmdline selects desktop=xorg (Xorg's\n\
              # platform probe of card0 hangs on this kernel's software-KMS).\n\
              # See linux-object fs/mod.rs. Nothing to do here.\n\
              # vt1: X takes the first VT directly (no udev/logind seat here).\n\
              exec startx -- vt1\n",
        )
        .unwrap();
        // PATH puts /usr/local/bin first, so these win over busybox applets.
        // Force path: `busybox reboot -f` is sync+reboot(2) and is the reboot
        // that actually works on this kernel. Signalling PID 1 used to run
        // eclipse-init's kill-all, which hung the GPU session. eclipse-init
        // now uses the same force path; the wrapper still execs busybox -f
        // so a typed `reboot` does not depend on init's event loop.
        fs::write(
            localbin.join("reboot"),
            b"#!/bin/sh\n\
              if [ -x /bin/busybox ]; then exec /bin/busybox reboot -f; fi\n\
              kill -INT 1\n",
        )
        .unwrap();
        fs::write(
            localbin.join("poweroff"),
            b"#!/bin/sh\n\
              if [ -x /bin/busybox ]; then exec /bin/busybox poweroff -f; fi\n\
              kill -TERM 1\n",
        )
        .unwrap();
        fs::write(
            localbin.join("halt"),
            b"#!/bin/sh\n\
              if [ -x /bin/busybox ]; then exec /bin/busybox halt -f; fi\n\
              kill -TERM 1\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            for w in [
                "eclipse-udhcpc",
                "eclipse-dbus",
                "eclipse-dbus-system",
                "eclipse-seatd",
                "eclipse-xorg",
                "eclipse-lunarbg",
                "eclipse-lunarbar",
                "eclipse-lunarclip",
                "eclipse-boot-sound",
                "eclipse-boot-sound-play",
                "eclipse-pulseaudio",
                "reboot",
                "poweroff",
                "halt",
            ] {
                let _ = fs::set_permissions(localbin.join(w), fs::Permissions::from_mode(0o755));
            }
            // Belt and braces: /usr/local/bin holds nothing but wrappers, so
            // every regular file in it must be executable. The list above is a
            // list, and `eclipse-oopslog` was added to the writes and NOT to
            // the list, which shipped it 0644; `execve` then failed with EACCES
            // and eclipse-init respawned the service for the whole boot. This
            // pass makes the next omission harmless instead of a respawn storm.
            if let Ok(entries) = fs::read_dir(localbin) {
                for entry in entries.flatten() {
                    // `is_file()` on the entry's own type, not the path's:
                    // following a symlink here would chmod whatever it points
                    // at, outside this directory.
                    if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        let _ =
                            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755));
                    }
                }
            }
        }
    }

    /// 从安装目录拷贝所有 so 和 so 链接到 rootfs
    fn put_libs(&self, musl: impl AsRef<Path>, dir: impl AsRef<Path>) {
        let lib = self.path().join("lib");
        let musl_libc_protected = format!("ld-musl-{}.so.1", self.0.name());
        let musl_libc_ignored = "libc.so";
        let strip = self.strip(musl);
        dir.as_ref()
            .join("lib")
            .read_dir()
            .unwrap()
            .filter_map(|res| res.map(|e| e.path()).ok())
            .filter(|path| check_so(path))
            .for_each(|source| {
                let name = source.file_name().unwrap();
                let target = lib.join(name);
                if source.is_symlink() {
                    if name != musl_libc_protected.as_str() {
                        // `fs::copy` 会拷贝文件内容
                        let Some(link_to) = rootfs_link_target(&source) else {
                            return;
                        };
                        dir::rm(&target).unwrap();
                        unix::fs::symlink(link_to, target).unwrap();
                    }
                } else if name != musl_libc_ignored {
                    dir::rm(&target).unwrap();
                    fs::copy(source, &target).unwrap();
                    Ext::new(&strip).arg("-s").arg(target).status();
                }
            });
    }
}

/// 为 PATH 环境变量附加路径。
/// ¿Cabe el cierre de apk del escritorio en la imagen de ESTE arco?
///
/// El cierre (Xorg + Mesa/LLVM + Firefox + XFCE + fuentes) pesa cerca de un
/// gigabyte instalado, y en todo lo que no es x86_64 la imagen que arranca
/// QEMU **es el rootfs entero**: `image()` la dimensiona con
/// `sfs_size_for(sfs_payload_bytes(rootfs))`, un 40% por encima del arbol.
/// x86_64 no tiene ese problema porque fotografia un root minimo ANTES de
/// copiar el escritorio y el cierre se va a `rootfs.btrfs.gz`.
///
/// Y de los dos que quedan, solo uno tiene un techo: `zCore/Makefile` le pasa
/// la imagen de riscv64 **como `-initrd` con `-m 1G`**, asi que la imagen vive
/// dentro de la RAM del invitado. Con el cierre dentro sale de 1,32 GiB y QEMU
/// ni arranca -- dice «Some ROM regions are overlapping» y se va con estado 2,
/// que es lo que le paso a `Linux Libc Test Baremetal (riscv64)` en cuanto el
/// #1758 dejo de saltarse el paso por arco. aarch64 la monta por virtio-blk
/// (`-drive file=aarch64.img`), fuera de la RAM, y ahi si cabe: su ISO de
/// escritorio la necesita.
///
/// No es el corte por arco que quito el #1758: aquello se saltaba TODO lo que
/// no fuera x86_64 -- y dejaba la desktop de aarch64 sin un solo paquete -- por
/// no tener un apk que arrancase en el host. Esto es un techo de tamaño de un
/// solo arco, con su motivo escrito.
fn apk_closure_fits_image(arch: &str) -> bool {
    arch != "riscv64"
}

fn join_path_env<I, S>(paths: I) -> OsString
where
    I: IntoIterator<Item = S>,
    S: AsRef<Path>,
{
    // `env::var` hands back `Err` for a `PATH` that is not valid UTF-8, and the
    // whole inherited `PATH` was then dropped on the floor; `var_os` keeps the
    // bytes. Reading the environment is all this wrapper does, so the assembling
    // below can be asked about without a test having to touch the environment of
    // the process it shares with every other test.
    join_path_env_from(env::var_os("PATH"), paths)
}

fn join_path_env_from<I, S>(inherited: Option<OsString>, paths: I) -> OsString
where
    I: IntoIterator<Item = S>,
    S: AsRef<Path>,
{
    let mut path = OsString::new();
    let mut first = true;
    // An EMPTY `PATH` is set-but-empty, so it used to count as a first element:
    // the result then began with a `:`, and an empty element in `PATH` means THE
    // CURRENT DIRECTORY to every exec that inherits it. Unset and empty have to
    // be treated the same.
    if let Some(current) = inherited.filter(|c| !c.is_empty()) {
        path.push(current);
        first = false;
    }
    for item in paths {
        if first {
            first = false;
        } else {
            path.push(":");
        }
        // `canonicalize` fails on a directory that does not exist yet, and the
        // panic named neither the path nor `PATH` -- just "No such file or
        // directory (os error 2)" from somewhere inside a build. An element of
        // `PATH` that does not exist is simply skipped by exec, so keep going
        // with the path as it was given and say which one it was.
        let item = item.as_ref();
        match item.canonicalize() {
            Ok(absolute) => path.push(absolute.as_os_str()),
            Err(e) => {
                eprintln!(
                    "warning: PATH entry {} could not be resolved: {e}",
                    item.display()
                );
                path.push(item.as_os_str());
            }
        }
    }
    path
}

/// The symlink to install in the rootfs for the library symlink `source`, or
/// `None` when there is nothing safe to install.
///
/// What goes in is the RAW link target, not the resolved path: the rootfs is
/// assembled on the host and mounted at a different root in the guest, so an
/// absolute target here names a host path that does not exist there. The result
/// is a dangling `/lib` entry whose only symptom is the loader saying "not
/// found" about a library that is visibly present.
///
/// A relative target already survives the move. An absolute one that names a
/// sibling in the same lib directory says the same thing as its file name, and
/// that sibling is installed by the same pass, so the name is the rewrite. An
/// absolute target pointing anywhere else has nothing to be rewritten to.
fn rootfs_link_target(source: &Path) -> Option<PathBuf> {
    let raw = source.read_link().ok()?;
    if raw.is_relative() {
        return Some(raw);
    }
    // The sibling has to be a DIFFERENT file: when the absolute target's last
    // component is the link's own name, rewriting to that name points the link
    // at itself, and `exists()` says yes because it follows the link back to the
    // host path. A self-referencing symlink is ELOOP for every reader.
    let sibling = raw
        .file_name()
        .map(|base| (base, source.with_file_name(base)));
    match sibling {
        Some((base, sibling)) if sibling != *source && sibling.exists() => {
            Some(PathBuf::from(base))
        }
        _ => {
            eprintln!(
                "warning: skipping {}: it is a symlink to the host path {}, \
                 which does not exist in the guest",
                source.display(),
                raw.display()
            );
            None
        }
    }
}

/// 判断一个文件是动态库或动态库的符号链接。
fn check_so<P: AsRef<Path>>(path: P) -> bool {
    let path = path.as_ref();
    // 是符号链接或文件
    // 对于符号链接，`is_file` `exist` 等函数都会针对其指向的真实文件判断
    if !path.is_symlink() && !path.is_file() {
        return false;
    }
    // 对文件名分段
    let name = path.file_name().unwrap().to_string_lossy();
    let mut seg = name.split('.');
    // 不能以 . 开头
    if matches!(seg.next(), Some("") | None) {
        return false;
    }
    // 扩展名的第一项是 so
    if !matches!(seg.next(), Some("so")) {
        return false;
    }
    // so 之后全是纯十进制数字
    //
    // Every segment after `so` is a version number, so it must be non-empty as
    // well as all digits: `all` over an EMPTY segment is vacuously true, which
    // made `libfoo.so.` answer yes and put a name no loader will ever look up
    // into the rootfs's /lib.
    seg.all(|it| !it.is_empty() && it.chars().all(|ch| ch.is_ascii_digit()))
}

#[cfg(test)]
mod var_run_tests {
    use super::*;

    /// `#!/usr/bin/env <cmd>` is the most common shebang in existence, and
    /// Eclipse shipped with no `/usr/bin/env` -- only `/bin/env` -- so every
    /// script using it failed to exec. The link must be relative (the rootfs is
    /// assembled on the host and mounted at a different root in the guest) and
    /// must point at busybox, which dispatches on argv[0].
    #[test]
    fn usr_bin_env_is_linked_to_busybox() {
        let dir =
            std::env::temp_dir().join(format!("eclipse-usrbinenv-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        LinuxRootfs::ensure_busybox_applets(&bin);

        let link = dir.join("usr/bin/env");
        assert!(
            link.is_symlink(),
            "no /usr/bin/env: every `#!/usr/bin/env ...` script fails to exec"
        );
        let target = fs::read_link(&link).unwrap();
        assert!(
            target.is_relative(),
            "/usr/bin/env must not point at a host-absolute path: {target:?}"
        );
        // Resolved from /usr/bin, the target must name the rootfs's busybox.
        assert_eq!(
            dir.join("usr/bin").join(&target),
            dir.join("usr/bin/../../bin/busybox"),
            "the link must resolve to /bin/busybox inside the rootfs, not {target:?}"
        );
        // And the applet it shadows in /bin is still there.
        assert!(bin.join("env").is_symlink());
        let _ = fs::remove_dir_all(&dir);
    }

    /// `/usr/local/bin/eclipse-oopslog` shipped **0644** from the day it was
    /// added: the script was written next to the other wrappers but never added
    /// to the `chmod 0755` list beside them. `oopslog.service` is
    /// `type = respawn`, so on every boot eclipse-init forked, `execve` failed
    /// with EACCES, the child `_exit(127)`ed in well under a millisecond, and
    /// the supervisor respawned it -- backing off to MAX_BACKOFF and then
    /// printing an `exit 127` line every 8 s for the rest of the boot. That is
    /// the "the service resets over and over" report from real hardware.
    ///
    /// The x bit is the whole fix, so it is what this asserts; the rest checks
    /// the script is shell the image's /bin/sh will accept and that the service
    /// file actually points at the file being chmodded.
    #[test]
    fn the_oopslog_wrapper_is_installed_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("eclipse-oopslog-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let localbin = dir.join("usr/local/bin");
        let svc_dir = dir.join("etc/eclipse/services");
        fs::create_dir_all(&localbin).unwrap();
        fs::create_dir_all(&svc_dir).unwrap();

        LinuxRootfs::write_oopslog(&localbin, &svc_dir);

        let script = localbin.join("eclipse-oopslog");
        let mode = fs::metadata(&script).unwrap().permissions().mode();
        assert_ne!(
            mode & 0o111,
            0,
            "eclipse-oopslog is {:04o}: execve fails with EACCES and init respawns it forever",
            mode & 0o7777
        );

        let src = fs::read_to_string(&script).unwrap();
        assert!(src.starts_with("#!/bin/sh\n"), "shebang");
        let st = std::process::Command::new("sh")
            .arg("-n")
            .arg(&script)
            .status()
            .unwrap();
        assert!(st.success(), "sh -n rejected eclipse-oopslog");

        // The loop must never fall out of the bottom: `type = respawn` would
        // make a script that returns look exactly like the bug above.
        assert!(src.contains("while :; do"), "the drain has to be a loop");

        let unit = fs::read_to_string(svc_dir.join("oopslog.service")).unwrap();
        assert!(
            unit.contains("exec = /usr/local/bin/eclipse-oopslog"),
            "the service must point at the script this chmods: {unit}"
        );
        assert!(unit.contains("type = respawn"), "{unit}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The drain must write each contained fault ONCE, however many times
    /// `/proc/oops` is read and however many reboots the file outlives.
    ///
    /// `/proc/oops` hands back the whole record since boot, not the part that
    /// is new, and `/var/log/oops.log` is appended to across reboots. Writing
    /// the snapshot whenever it changed therefore wrote fault #1 again when
    /// fault #2 arrived, and wrote the lot again after a reboot under a fresh
    /// `=== date ===` header. That is not a tidiness problem: two captures of
    /// one bug came back carrying a `[null-exec]` block identical down to
    /// `r14=0x3b`, from different boots, and were read as one fault beside a
    /// `[KERNEL PAGE FAULT]` that had nothing to do with it.
    ///
    /// This drives the shipped script's own loop body over a `/proc/oops` that
    /// grows the way a real one does.
    #[test]
    fn the_drain_never_writes_the_same_fault_twice() {
        let dir = std::env::temp_dir().join(format!("eclipse-oops-dedup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let localbin = dir.join("usr/local/bin");
        let svc_dir = dir.join("etc/eclipse/services");
        fs::create_dir_all(&localbin).unwrap();
        fs::create_dir_all(&svc_dir).unwrap();
        LinuxRootfs::write_oopslog(&localbin, &svc_dir);

        // The record as the kernel grows it: one fault, then two.
        let first = "[isolate] fault one contained\n";
        let second = "[isolate] fault one contained\n[isolate] fault two contained\n";
        let proc_oops = dir.join("proc_oops");
        let stage2 = dir.join("stage2");
        fs::write(&proc_oops, first).unwrap();
        fs::write(&stage2, second).unwrap();
        let out = dir.join("oops.log");

        // The shipped text, with the three things a test cannot have: the real
        // /proc path, the real /var/log path, and a loop that never ends.
        let src = fs::read_to_string(localbin.join("eclipse-oopslog")).unwrap();
        let driver = src
            .replace("OUT=/var/log/oops.log", &format!("OUT={}", out.display()))
            .replace("mkdir -p /var/log 2>/dev/null", "ITER=0")
            .replace(
                "cat /proc/oops 2>/dev/null",
                &format!("cat {}", proc_oops.display()),
            )
            .replace(
                " sleep 10",
                &format!(
                    " ITER=$((ITER+1)); [ $ITER -ge 2 ] && break; cp {} {}",
                    stage2.display(),
                    proc_oops.display()
                ),
            );
        assert!(
            driver.contains("break"),
            "the sleep was not where this expected it; the driver would loop forever"
        );
        let script = dir.join("driver.sh");
        fs::write(&script, &driver).unwrap();

        let st = std::process::Command::new("sh")
            .arg(&script)
            .status()
            .unwrap();
        assert!(st.success(), "the drain exited non-zero");

        let log = fs::read_to_string(&out).unwrap();
        assert_eq!(
            log.matches("fault one contained").count(),
            1,
            "fault one was written again when fault two arrived:\n{log}"
        );
        assert_eq!(
            log.matches("fault two contained").count(),
            1,
            "fault two is missing or doubled:\n{log}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The session-bus wrapper must be valid POSIX shell, must prefer Alpine's
    /// dbus-daemon over Eclipse's own daemon, must bind the SAME address every
    /// session already exports (`unix:path=/run/user/0/bus`) -- a wrapper that
    /// binds anywhere else is a bus no client will ever find -- and must say
    /// something findable when neither daemon is installed, because init wires
    /// a service's stdio to /dev/null.
    #[test]
    fn dbus_wrapper_parses_and_prefers_dbus_daemon() {
        let dir =
            std::env::temp_dir().join(format!("eclipse-dbuswrap-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        LinuxRootfs::write_dbus_wrapper(&dir);

        let path = dir.join("eclipse-dbus");
        let src = fs::read_to_string(&path).unwrap();
        assert!(src.starts_with("#!/bin/sh\n"), "shebang");
        let st = std::process::Command::new("sh")
            .arg("-n")
            .arg(&path)
            .status()
            .unwrap();
        assert!(st.success(), "sh -n rejected eclipse-dbus");

        assert!(
            src.find("dbus-daemon").unwrap() < src.find("eclipse-dbusd").unwrap(),
            "Alpine's dbus-daemon must be tried first"
        );
        // Both daemons must be launched in the FOREGROUND: init supervises
        // them, and a daemon that forks away is one init would respawn forever.
        assert!(
            src.contains("--nofork"),
            "dbus-daemon must stay in the foreground"
        );
        assert!(!src.contains("--fork"), "no forking daemon under init");
        // The address every session exports, and nothing else.
        assert_eq!(
            src.matches("unix:path=$BUS").count(),
            4,
            "each daemon's log line and its --address use the one address"
        );
        assert!(src.contains("BUS=\"$XDG_RUNTIME_DIR/bus\""));
        assert!(
            src.contains("/etc/machine-id"),
            "dbus validates the machine id"
        );
        assert!(
            src.contains("> /dev/console"),
            "a missing daemon must be findable"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Every `module-alsa-sink` argument the generated system.pa passes must
    /// be one the module accepts. pa_modargs rejects a load-module line as a
    /// whole on the first unknown key, and the guest log then reads
    /// "Failed to parse module arguments" with no sink created -- which is
    /// how `mixer_device=` / `use_ucm=` (module-alsa-card keys) silenced
    /// PulseAudio entirely. Catch the next one at build time, not in the
    /// guest. The list is src/modules/alsa/module-alsa-sink.c's valid_modargs.
    #[test]
    fn system_pa_alsa_sink_args_are_all_accepted() {
        const ACCEPTED: &[&str] = &[
            "name",
            "sink_name",
            "sink_properties",
            "namereg_fail",
            "device",
            "device_id",
            "format",
            "rate",
            "alternate_rate",
            "channels",
            "channel_map",
            "fragments",
            "fragment_size",
            "mmap",
            "tsched",
            "tsched_buffer_size",
            "tsched_buffer_watermark",
            "ignore_dB",
            "control",
            "rewind_safeguard",
            "deferred_volume",
            "deferred_volume_safety_margin",
            "deferred_volume_extra_delay",
            "fixed_latency_range",
        ];
        let dir =
            std::env::temp_dir().join(format!("eclipse-pulseconf-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        LinuxRootfs::write_pulse_conf(&dir);
        let pa = fs::read_to_string(dir.join("etc/pulse/system.pa")).unwrap();
        let mut sinks = 0;
        for line in pa
            .lines()
            .filter(|l| l.contains("load-module module-alsa-sink"))
        {
            sinks += 1;
            for arg in line.split_whitespace().skip(2) {
                let key = arg.split('=').next().unwrap();
                assert!(
                    ACCEPTED.contains(&key),
                    "system.pa passes `{key}` to module-alsa-sink, which does not accept it: {line}"
                );
            }
        }
        assert!(
            sinks >= 1,
            "system.pa must load at least one module-alsa-sink"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `/var/run` must end up as the FHS symlink to `/run` even when an older
    /// build left it as a real directory holding `pulse/`; creating
    /// `var/run/pulse` afterwards must land in `run/pulse` (one directory
    /// behind both paths is the whole point); and a second call is a no-op.
    #[test]
    fn ensure_var_run_replaces_a_real_dir_with_the_fhs_symlink() {
        let dir = std::env::temp_dir().join(format!("eclipse-varrun-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // An older build: a real var/run with runtime state inside, no run/.
        fs::create_dir_all(dir.join("var/run/pulse")).unwrap();
        assert!(!dir.join("run").exists());

        LinuxRootfs::ensure_var_run(&dir);

        let link = dir.join("var/run");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "var/run must be a symlink, not a directory"
        );
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("../run"));
        assert!(
            dir.join("run").is_dir(),
            "run/ must exist so the link never dangles"
        );

        // What write_pulse_conf does next: through the link, into run/pulse.
        fs::create_dir_all(dir.join("var/run/pulse")).unwrap();
        assert!(
            dir.join("run/pulse").is_dir(),
            "var/run/pulse must resolve to run/pulse"
        );

        // Idempotent: the link is kept, not replaced or nested.
        LinuxRootfs::ensure_var_run(&dir);
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("../run"));
        assert!(dir.join("run/pulse").is_dir());

        // A symlink to the WRONG place (or dangling) is not "already done":
        // it must be replaced by the FHS link, not accepted.
        fs::remove_file(&link).unwrap();
        unix::fs::symlink("../../tmp", &link).unwrap();
        LinuxRootfs::ensure_var_run(&dir);
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("../run"));
        fs::remove_file(&link).unwrap();
        unix::fs::symlink("../nowhere", &link).unwrap();
        LinuxRootfs::ensure_var_run(&dir);
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("../run"));

        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod fstab_template_tests {
    use super::*;

    /// The btrfs template is patched in place by `install-eclipse`, which
    /// looks for each line exactly as its `FSTAB_TEMPLATE_LINE_*` says: a
    /// column moved by one space and the installed system boots with a
    /// placeholder for a device.
    #[test]
    fn the_btrfs_template_has_the_lines_the_installer_patches() {
        let installer =
            fs::read_to_string(PROJECT_DIR.join("tools/install-eclipse/install-eclipse.c"))
                .unwrap();
        let template = LinuxRootfs::fstab_template(image::DiskFs::Btrfs);
        let mut found = 0;
        for line in installer.lines() {
            let Some(rest) = line.strip_prefix("#define FSTAB_TEMPLATE_LINE_") else {
                continue;
            };
            let quoted = rest.split('"').nth(1).unwrap();
            assert!(
                template.lines().any(|l| l == quoted),
                "the template has no line {quoted:?}"
            );
            found += 1;
        }
        assert_eq!(found, 4, "ROOT, EFI, HOME and SWAP");
    }

    /// With EclipseFS images, the disk lines say so (the installer rewrites
    /// the file by mounting the partition, so only the type matters).
    #[test]
    fn the_eclipsefs_template_names_eclipsefs_for_root_and_home() {
        let template = LinuxRootfs::fstab_template(image::DiskFs::EclipseFs);
        let types: Vec<&str> = template
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split_whitespace().nth(2).unwrap())
            .collect();
        assert_eq!(types, ["eclipsefs", "vfat", "eclipsefs", "swap"]);
    }
}

#[cfg(test)]
mod lunar_client_tests {
    use super::*;

    /// Every Wayland client that binds the foreign-toplevel manager must tell
    /// wayland-client what object its `toplevel` event creates. Without that
    /// specialization the client aborts on the first window it is told about,
    /// which is a crash that only shows up under a real compositor: it needs
    /// another window to exist, so neither a build nor a `--dump` catches it.
    #[test]
    fn wayland_clients_specialize_the_toplevel_event() {
        let dir = PROJECT_DIR.join("tools").join("lunarbar").join("src");
        for rel in ["main.rs", "bin/lunarrun.rs"] {
            let src = fs::read_to_string(dir.join(rel)).unwrap();
            if !src.contains("ZwlrForeignToplevelManagerV1") {
                continue;
            }
            assert!(
                src.contains("event_created_child!"),
                "{rel} binds zwlr_foreign_toplevel_manager_v1 without an \
                 event_created_child! specialization; it will abort as soon \
                 as a window exists"
            );
        }
    }
}

#[cfg(test)]
mod variant_layout_tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-variant-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The `desktop` variant has to keep EVERY historical path. `make qemu`,
    /// `scripts/qemu-bench.sh`, `tools/x11-bench/run.sh` and the zCore Makefile
    /// all name `rootfs/x86_64` and `ignored/target/efi.img.gz` literally. A
    /// suffix here (`rootfs/x86_64-desktop`) would leave them pointing at a tree
    /// nobody writes any more, and nothing would say so: the build would
    /// succeed and the image would be the one from before the change.
    #[test]
    fn la_variante_de_escritorio_conserva_todas_las_rutas_historicas() {
        let desktop = LinuxRootfs::new(Arch::X86_64);
        assert_eq!(desktop.path(), PROJECT_DIR.join("rootfs").join("x86_64"));
        assert_eq!(desktop.artifact("efi", "img.gz"), TARGET.join("efi.img.gz"));
        assert_eq!(
            desktop.artifact("rootfs", "btrfs.gz"),
            TARGET.join("rootfs.btrfs.gz")
        );
        assert_eq!(
            desktop.artifact("iso-initramfs", "img"),
            TARGET.join("iso-initramfs.img")
        );
        assert_eq!(
            desktop.artifact_dir("live-rootfs"),
            TARGET.join("live-rootfs")
        );
        // `new` is the one every existing caller uses, so it must mean desktop.
        assert_eq!(
            desktop.path(),
            LinuxRootfs::with_variant(Arch::X86_64, Variant::Desktop).path()
        );
    }

    /// And `minimal` must share NOTHING with it. Two variants writing the same
    /// `rootfs.btrfs.gz` is how `make release` would package the desktop
    /// payload inside the minimal ISO: the build is green, the ISO boots, and
    /// the installed system is the wrong one.
    #[test]
    fn la_minimal_no_comparte_ni_una_ruta_con_la_de_escritorio() {
        let desktop = LinuxRootfs::new(Arch::X86_64);
        let minimal = LinuxRootfs::with_variant(Arch::X86_64, Variant::Minimal);

        assert_eq!(
            minimal.path(),
            PROJECT_DIR.join("rootfs").join("x86_64-minimal")
        );
        assert_ne!(desktop.path(), minimal.path());
        for (stem, ext) in [
            ("efi", "img"),
            ("efi", "img.gz"),
            ("initramfs", "img"),
            ("iso-initramfs", "img"),
            ("rootfs", "btrfs"),
            ("rootfs", "btrfs.gz"),
            ("home", "btrfs"),
            ("home", "btrfs.gz"),
        ] {
            assert_ne!(
                desktop.artifact(stem, ext),
                minimal.artifact(stem, ext),
                "{stem}.{ext} lo escriben las dos variantes"
            );
        }
        assert_ne!(
            desktop.artifact_dir("live-rootfs"),
            minimal.artifact_dir("live-rootfs")
        );
        // The suffix goes BEFORE the extension, so the file keeps its type:
        // `efi-minimal.img.gz` stays a .img.gz, and `make iso` can still glob
        // or name it by extension.
        assert_eq!(
            minimal.artifact("efi", "img.gz"),
            TARGET.join("efi-minimal.img.gz")
        );
    }

    /// The arch still separates two rootfs of the same variant: `make release`
    /// walks arch × variant, and four combinations need four trees.
    #[test]
    fn la_arquitectura_sigue_separando_dentro_de_una_variante() {
        let x86 = LinuxRootfs::with_variant(Arch::X86_64, Variant::Minimal);
        let arm = LinuxRootfs::with_variant(Arch::Aarch64, Variant::Minimal);
        assert_eq!(
            arm.path(),
            PROJECT_DIR.join("rootfs").join("aarch64-minimal")
        );
        assert_ne!(x86.path(), arm.path());
    }

    /// La imagen que arrancan los lanzadores. `desktop` tiene que seguir
    /// siendo `zCore/x86_64.img`: es la ruta que nombran `make qemu`,
    /// `zCore/Makefile`, `scripts/qemu-bench.sh` y `tools/x11-bench/run.sh`.
    #[test]
    fn la_imagen_viva_de_escritorio_conserva_su_ruta_historica() {
        let desktop = LinuxRootfs::new(Arch::X86_64);
        assert_eq!(
            desktop.live_image(),
            PROJECT_DIR.join("zCore").join("x86_64.img")
        );
        assert_eq!(
            LinuxRootfs::with_variant(Arch::Aarch64, Variant::Desktop).live_image(),
            PROJECT_DIR.join("zCore").join("aarch64.img")
        );
    }

    /// Y la minimal tiene que apuntar a OTRO fichero, en las dos
    /// arquitecturas. Si las dos variantes dieran la misma ruta, `cargo qemu
    /// --variant minimal` arrancaría la de escritorio sin decir nada, que es
    /// justo lo que hacía.
    #[test]
    fn la_imagen_viva_minimal_no_es_la_de_escritorio() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let desktop = LinuxRootfs::with_variant(arch, Variant::Desktop).live_image();
            let minimal = LinuxRootfs::with_variant(arch, Variant::Minimal).live_image();
            assert_ne!(desktop, minimal, "{}", arch.name());
            assert_eq!(
                minimal,
                PROJECT_DIR
                    .join("zCore")
                    .join(format!("{}-minimal.img", arch.name())),
                "{}",
                arch.name()
            );
        }
    }

    /// `eclipse-init` with no `/etc/eclipse/desktop` defaults to `labwc` (see
    /// `selected_desktop` in tools/eclipse-init). On the minimal image that
    /// means a service respawning a compositor that was never installed, so the
    /// file is not a nicety: it is what makes the minimal variant boot to a
    /// console instead of to a retry loop.
    #[test]
    fn la_minimal_deja_escrito_que_no_hay_sesion_grafica() {
        let dir = scratch("desktop-none");
        LinuxRootfs::write_desktop_session(&dir, "none");
        let written = fs::read_to_string(dir.join("etc/eclipse/desktop")).unwrap();
        // The first whitespace token is what init reads.
        assert_eq!(written.split_whitespace().next(), Some("none"));
        // Terminated, so an `echo labwc >>` cannot produce `nonelabwc`.
        assert!(
            written.ends_with('\n'),
            "{written:?} sin salto de línea final"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// It has to create `etc/eclipse` itself: the minimal rootfs reaches this
    /// point with an `etc` that no one has put an `eclipse` directory into, and
    /// a silently skipped write is a minimal image that boots looking for labwc.
    #[test]
    fn escribe_la_sesion_aunque_no_exista_el_directorio() {
        let dir = scratch("desktop-mkdir");
        assert!(!dir.join("etc").exists());
        LinuxRootfs::write_desktop_session(&dir, "none");
        assert!(dir.join("etc/eclipse/desktop").is_file());
        let _ = fs::remove_dir_all(&dir);
    }
}

/// De que arquitectura baja apk los paquetes. Lo decide `/etc/apk/arch`
/// dentro del rootfs, no `/etc/apk/repositories`: ese solo nombra la rama
/// (`.../v3.24/main`) y apk le pega el arco detras. Sin ese fichero apk cae a
/// su arco compilado — el del HOST en una compilacion cruzada.
#[cfg(test)]
mod apk_keys_tests {
    use super::*;

    fn scratch(que: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "eclipse-apk-keys-{que}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn claves_de(arco: &str) -> Vec<String> {
        let dir = PROJECT_DIR
            .join("tools")
            .join("apk")
            .join("keys")
            .join(arco);
        let mut v: Vec<String> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("falta {}: {e}", dir.display()))
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".pub"))
            .collect();
        v.sort();
        v
    }

    /// Alpine firma el APKINDEX de **cada arquitectura con una clave
    /// distinta**, asi que cada arco que Eclipse compila tiene que traer las
    /// suyas. El reparto sale del `_arch_keys` del APKBUILD de `alpine-keys`.
    #[test]
    fn cada_arco_trae_sus_claves() {
        for (arco, esperadas) in [
            ("x86_64", vec!["4a6a0840", "5261cecb", "6165ee59"]),
            ("aarch64", vec!["58199dcc", "616ae350"]),
            ("riscv64", vec!["60ac2099", "616db30d"]),
        ] {
            let tiene = claves_de(arco);
            assert_eq!(tiene.len(), esperadas.len(), "claves de {arco}: {tiene:?}");
            for id in esperadas {
                assert!(
                    tiene.iter().any(|n| n.contains(id)),
                    "a {arco} le falta la clave {id}: {tiene:?}"
                );
            }
        }
    }

    /// Dos arcos no comparten clave (salvo la de x86/x86_64, que aqui no
    /// aplica): si la lista de uno se colara en la del otro, el aviso de
    /// «UNTRUSTED signature» volveria sin que nada se queje.
    #[test]
    fn las_claves_de_un_arco_no_son_las_de_otro() {
        let x86 = claves_de("x86_64");
        let arm = claves_de("aarch64");
        let risc = claves_de("riscv64");
        for (a, b, n) in [
            (&x86, &arm, "x86_64/aarch64"),
            (&arm, &risc, "aarch64/riscv64"),
        ] {
            assert!(
                !a.iter().any(|k| b.contains(k)),
                "{n} comparten clave, y Alpine no las comparte"
            );
        }
    }

    /// Lo que se instala en el rootfs son las sueltas MAS las del objetivo, y
    /// nunca las de otro arco.
    #[test]
    fn se_instalan_las_sueltas_y_las_del_objetivo() {
        let dst = scratch("instala").join("keys");
        let n = LinuxRootfs::install_apk_keys(&dst, "aarch64");
        let puestas: Vec<String> = fs::read_dir(&dst)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(n, puestas.len(), "la cuenta es lo que apk va a confiar");
        for k in claves_de("aarch64") {
            assert!(puestas.contains(&k), "falta {k} en {puestas:?}");
        }
        for k in claves_de("riscv64") {
            assert!(
                !puestas.contains(&k),
                "{k} es de riscv64 y no pinta nada en un rootfs de aarch64"
            );
        }
    }

    /// El fallo que hubo que arreglar, en su forma exacta: un directorio con
    /// claves de OTRO arco cuenta como «hay claves», asi que nadie pasa
    /// `--allow-untrusted`, y apk se encuentra la firma del objetivo sin la
    /// clave que la verifica. Tiene que quedar una clave del arco pedido.
    #[test]
    fn un_rootfs_de_aarch64_no_se_queda_solo_con_claves_de_x86() {
        let dst = scratch("mezcla").join("keys");
        fs::create_dir_all(&dst).unwrap();
        for k in claves_de("x86_64") {
            fs::write(dst.join(&k), b"vieja\n").unwrap();
        }
        LinuxRootfs::install_apk_keys(&dst, "aarch64");
        let puestas: Vec<String> = fs::read_dir(&dst)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            claves_de("aarch64").iter().any(|k| puestas.contains(k)),
            "un rootfs de aarch64 sin una sola clave de aarch64: {puestas:?}"
        );
    }

    /// Las siete son claves publicas RSA de verdad, no ficheros de relleno.
    #[test]
    fn las_claves_son_claves_publicas_rsa() {
        for arco in ["x86_64", "aarch64", "riscv64"] {
            let dir = PROJECT_DIR
                .join("tools")
                .join("apk")
                .join("keys")
                .join(arco);
            for k in claves_de(arco) {
                let t = fs::read_to_string(dir.join(&k)).unwrap();
                assert!(
                    t.starts_with("-----BEGIN PUBLIC KEY-----")
                        && t.trim_end().ends_with("-----END PUBLIC KEY-----"),
                    "{arco}/{k} no es una clave publica PEM"
                );
            }
        }
    }
}

#[cfg(test)]
mod apk_arch_tests {
    use super::*;

    /// Misma convencion que `xorg::scratch`: el arbol en /tmp va etiquetado
    /// por proceso e hilo para que dos tests en paralelo no se pisen.
    fn scratch(que: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "eclipse-apk-arch-{que}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Lo que se escribe es el arco objetivo, tal cual y con salto de linea,
    /// que es como lo lee apk.
    #[test]
    fn el_arco_escrito_es_el_del_objetivo() {
        for arch in ["x86_64", "aarch64", "riscv64"] {
            let etc = scratch(arch).join("etc");
            LinuxRootfs::write_apk_arch(&etc, arch);
            let leido = fs::read_to_string(etc.join("apk").join("arch")).unwrap();
            assert_eq!(leido, format!("{arch}\n"), "arco de {arch}");
        }
    }

    /// Se escribe aunque `etc/apk` no exista todavia: en la ruta desde cero el
    /// directorio lo crea el bloque de apk, pero en la incremental no hay quien
    /// lo cree, y un `return` silencioso ahi dejaba el rootfs sin arco.
    #[test]
    fn crea_el_directorio_si_hace_falta() {
        let etc = scratch("crea").join("etc");
        assert!(!etc.exists());
        LinuxRootfs::write_apk_arch(&etc, "aarch64");
        assert!(etc.join("apk").join("arch").is_file());
    }

    /// Un segundo `cargo rootfs` para otro arco tiene que PISAR el valor, no
    /// dejar el de la tirada anterior de `make release`.
    #[test]
    fn una_segunda_tirada_pisa_el_arco_de_la_primera() {
        let etc = scratch("pisa").join("etc");
        LinuxRootfs::write_apk_arch(&etc, "x86_64");
        LinuxRootfs::write_apk_arch(&etc, "aarch64");
        assert_eq!(
            fs::read_to_string(etc.join("apk").join("arch")).unwrap(),
            "aarch64\n"
        );
    }

    /// La configuracion de apk no puede volver a colgar de que exista el
    /// estatico del objetivo: ese es best-effort (aqui mismo el de aarch64 no
    /// se baja) y lo que arma el cierre de paquetes es el apk del host. Iban
    /// juntos dentro de un `if apk.is_file()`, asi que un objetivo sin
    /// estatico salia ademas sin `/etc/apk/repositories`.
    #[test]
    fn la_configuracion_de_apk_no_cuelga_del_estatico_del_objetivo() {
        let src = include_str!("mod.rs");
        let i = src
            .find("if apk.is_file() {")
            .expect("sigue habiendo una comprobacion del estatico");
        let bloque = &src[i..];
        let fin = bloque
            .find("\n            }")
            .or_else(|| bloque.find("\n        }"))
            .unwrap_or(bloque.len());
        let dentro = &bloque[..fin];
        for aguja in ["repositories", "write_apk_arch", "install_apk_keys"] {
            assert!(
                !dentro.contains(aguja),
                "{aguja} ha vuelto a quedar dentro del `if apk.is_file()`"
            );
        }
    }
}

/// How the Rust musl userspace tools get linked. The whole batch exists
/// because `make release` on aarch64 printed five linker errors and carried
/// on: the tools are best-effort, so an arm64 image shipped without
/// eclipse-init, eclipse-dbusd, wavplay and lunarbar/lunarbg/lunarrun and
/// said so only in warnings. See `LinuxRootfs::cross_build_env`.
#[cfg(test)]
mod cross_linker_tests {
    use super::*;

    /// The host-native build must keep the exact command line it has always
    /// used. x86_64-on-x86_64 has been working for the whole life of the tree;
    /// pointing it at a downloaded cross gcc to fix arm64 would be trading one
    /// broken arch for another.
    #[test]
    fn el_host_nativo_no_toca_el_enlazador() {
        // A host Eclipse does not target at all (an ARMv7 builder, say) has no
        // native case to check; it must not fail the suite either.
        let Ok(host) = std::env::consts::ARCH.parse::<Arch>() else {
            return;
        };
        let rootfs = LinuxRootfs::new(host);
        assert!(
            !rootfs.needs_cross_linker(),
            "{} es el arch del host y no debería necesitar cross",
            host.name()
        );
        let env = rootfs.cross_build_env();
        assert_eq!(
            env,
            vec![("RUSTFLAGS".to_string(), STATIC_RUSTFLAGS.to_string())],
            "el caso nativo ha dejado de ser el de siempre"
        );
    }

    /// And every OTHER arch does need one, which is the actual bug: on this
    /// x86_64 host `aarch64-unknown-linux-musl` was linked by `/usr/bin/ld`,
    /// which rejects rustc's `--fix-cortex-a53-843419` outright.
    #[test]
    fn los_demas_arcos_si_necesitan_cross() {
        let host = std::env::consts::ARCH;
        for arch in [Arch::X86_64, Arch::Aarch64, Arch::Riscv64] {
            assert_eq!(
                LinuxRootfs::new(arch).needs_cross_linker(),
                arch.name() != host,
                "{} contra un host {host}",
                arch.name()
            );
        }
    }

    /// The cross case has to ADD the linker, not replace the flags: a tool
    /// linked PIE would not be the static non-PIE ET_EXEC the Eclipse loader
    /// and the busybox/apk base expect.
    #[test]
    fn el_enlazador_cruzado_no_pisa_el_relocation_model() {
        let rootfs = LinuxRootfs::new(Arch::Aarch64);
        let env = rootfs.cross_build_env_with(Some("/toolchain/bin/aarch64-linux-musl-gcc"));
        let rustflags = env
            .iter()
            .find(|(k, _)| k == "RUSTFLAGS")
            .map(|(_, v)| v.as_str())
            .expect("RUSTFLAGS");
        assert!(
            rustflags.contains("-C relocation-model=static"),
            "{rustflags:?} ha perdido el relocation-model"
        );
        assert!(
            rustflags.contains("-C linker=/toolchain/bin/aarch64-linux-musl-gcc"),
            "{rustflags:?} no lleva el enlazador cruzado"
        );
    }

    /// cc-rs reads `CC_<triple>` with the dashes turned into UNDERSCORES. With
    /// the dashes left in, the variable is never read at all and a dependency
    /// with a C build script goes back to the host compiler -- silently, which
    /// is exactly the failure mode this batch is about.
    #[test]
    fn la_variable_de_cc_va_con_guiones_bajos() {
        let env = LinuxRootfs::new(Arch::Aarch64).cross_build_env_with(Some("/cc"));
        assert!(
            env.iter()
                .any(|(k, v)| k == "CC_aarch64_unknown_linux_musl" && v == "/cc"),
            "no está CC_aarch64_unknown_linux_musl: {env:?}"
        );
        assert!(
            !env.iter().any(|(k, _)| k.contains('-')),
            "una variable de entorno con guiones no la lee nadie: {env:?}"
        );
    }

    /// No `CC_…` at all in the native case: it would point host build scripts
    /// at a compiler they must not use.
    #[test]
    fn sin_cross_no_se_fija_ningun_compilador() {
        let env = LinuxRootfs::new(Arch::X86_64).cross_build_env_with(None);
        assert!(
            !env.iter().any(|(k, _)| k.starts_with("CC")),
            "{env:?} fija un compilador sin hacer falta"
        );
    }

    /// Every arch, cross or native, links static. riscv64 is the one that
    /// needs saying: its musl target spec has no `crt-static-default`, so
    /// before this flag its `/sbin/init` was a dynamic binary that asked the
    /// rootfs for `_Unwind_*` out of a `libgcc_s.so` nobody ships, exited 127
    /// and left the arch unbootable.
    #[test]
    fn todos_los_arcos_enlazan_estatico() {
        for arch in [Arch::X86_64, Arch::Aarch64, Arch::Riscv64] {
            for cc in [None, Some("/toolchain/bin/cross-gcc")] {
                let env = LinuxRootfs::new(arch).cross_build_env_with(cc);
                let rustflags = env
                    .iter()
                    .find(|(k, _)| k == "RUSTFLAGS")
                    .map(|(_, v)| v.as_str())
                    .expect("RUSTFLAGS");
                assert!(
                    rustflags.contains("-C target-feature=+crt-static"),
                    "{} se enlazaria dinamico: {rustflags:?}",
                    arch.name()
                );
            }
        }
    }

    /// The recurrence, not the instance. The same `RUSTFLAGS` line was copied
    /// into five build functions, so the sixth tool would have copied it again
    /// and arrived with the same broken linker. Nothing may hand `RUSTFLAGS`
    /// to a command directly any more: the value comes from
    /// `cross_build_env`, which is the only thing that knows whether a cross
    /// linker is needed.
    #[test]
    fn ninguna_herramienta_arma_su_propio_rustflags() {
        let src = include_str!("mod.rs");
        let culprits: Vec<&str> = src
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("env(\"RUSTFLAGS\""))
            .collect();
        assert!(
            culprits.is_empty(),
            "estas líneas le pasan RUSTFLAGS a un comando a mano en vez de \
             usar cross_build_env: {culprits:?}"
        );
    }
}

#[cfg(test)]
mod rootfs_plumbing_tests;

#[cfg(test)]
mod hunter_schedule_tests;

#[cfg(test)]
mod apk_closure_tests {
    use super::apk_closure_fits_image;

    /// El arco que rompio: su imagen va de `-initrd` en un invitado de 1 GiB.
    #[test]
    fn riscv64_no_carga_el_cierre_de_apk() {
        assert!(
            !apk_closure_fits_image("riscv64"),
            "la imagen de riscv64 la carga QEMU en la RAM del invitado (-initrd, -m 1G); \
             con el cierre del escritorio dentro sale de 1,3 GiB y no arranca"
        );
    }

    /// Y el que NO hay que volver a dejar sin paquetes: el #1758 existe por eso.
    #[test]
    fn aarch64_si_lo_carga() {
        assert!(
            apk_closure_fits_image("aarch64"),
            "aarch64 monta su imagen por virtio-blk, fuera de la RAM, y su ISO de \
             escritorio necesita el cierre: dejarla sin el es el bug que arreglo el #1758"
        );
    }

    #[test]
    fn x86_64_si_lo_carga() {
        assert!(apk_closure_fits_image("x86_64"));
    }

    /// El techo es de UN arco, no «todo lo que no sea x86_64».
    #[test]
    fn no_es_un_corte_por_todo_lo_que_no_sea_x86() {
        let fuera: Vec<&str> = ["x86_64", "aarch64", "riscv64"]
            .into_iter()
            .filter(|a| !apk_closure_fits_image(a))
            .collect();
        assert_eq!(
            fuera,
            vec!["riscv64"],
            "solo riscv64 tiene techo de RAM; si aparece otro, que sea con su motivo escrito"
        );
    }
}

/// The newest modification time of any file under `dir`, recursively.
fn newest_mtime_under(dir: &Path) -> Option<std::time::SystemTime> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let ty = e.file_type().ok()?;
            if ty.is_dir() {
                newest_mtime_under(&e.path())
            } else {
                e.metadata().ok()?.modified().ok()
            }
        })
        .max()
}

#[cfg(test)]
mod newest_mtime_tests {
    use super::newest_mtime_under;
    use std::fs;
    use std::time::{Duration, SystemTime};

    /// A file in a subdirectory counts: `main_tests/` and any future
    /// submodule directory must trigger a rebuild like `src/` itself.
    #[test]
    fn a_nested_file_is_found_and_the_newest_wins() {
        let root = std::env::temp_dir().join(format!("newest-mtime-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        let old = root.join("top.rs");
        let new = root.join("a/b/deep.rs");
        fs::write(&old, "").unwrap();
        fs::write(&new, "").unwrap();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let t1 = t0 + Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(t0)
            .unwrap();
        fs::File::options()
            .write(true)
            .open(&new)
            .unwrap()
            .set_modified(t1)
            .unwrap();
        assert_eq!(newest_mtime_under(&root), Some(t1));
        assert_eq!(newest_mtime_under(&root.join("missing")), None);
        fs::remove_dir_all(&root).unwrap();
    }
}
