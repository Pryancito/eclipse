mod btrfs_image;
mod image;
mod opencv;
mod test;

use crate::{commands::fetch_online, Arch, PROJECT_DIR, REPOS};
use os_xtask_utils::{dir, CommandExt, Ext, Git, Make};
use std::{
    env,
    ffi::OsString,
    fs,
    os::unix,
    path::{Path, PathBuf},
};

pub(crate) struct LinuxRootfs(Arch);

impl LinuxRootfs {
    /// 生成指定架构的 linux rootfs 操作对象。
    #[inline]
    pub const fn new(arch: Arch) -> Self {
        Self(arch)
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
            // resize2fs/e2fsck/mke2fs (para expandir ROOT y formatear HOME).
            self.install_e2fsprogs_bins(&musl, &bin);
            self.install_thread_tests(&dir);
            // INIT (PID 1): OpenRC by default, with busybox init as a resilient
            // fallback. `install_busybox_init` runs first so `/sbin/init` always
            // resolves to *some* PID 1; `install_openrc` then repoints it at
            // `openrc-init` when the (best-effort) OpenRC build is available.
            Self::install_base_accounts(&dir);
            self.install_busybox_init(&dir);
            self.install_openrc(&dir, &musl);
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

        let apk = self.apk(&musl);
        if apk.is_file() {
            fs::copy(&apk, bin.join("apk")).unwrap();
            let etc = dir.join("etc");
            let etc_apk = etc.join("apk");
            fs::create_dir_all(&etc_apk).unwrap();
            fs::write(
                etc_apk.join("repositories"),
                "http://dl-cdn.alpinelinux.org/alpine/v3.23/main\nhttp://dl-cdn.alpinelinux.org/alpine/v3.23/community\n",
            )
            .unwrap();
            fs::write(etc_apk.join("world"), "").unwrap();

            // Alpine repo signatures: without /etc/apk/keys/*.rsa.pub apk reports
            // "UNTRUSTED signature" and leaves 0 packages after a slow index download.
            let keys_dst = etc_apk.join("keys");
            fs::create_dir_all(&keys_dst).unwrap();
            let keys_src = PROJECT_DIR.join("prebuilt").join("alpine-apk-keys");
            if keys_src.is_dir() {
                for entry in fs::read_dir(&keys_src).unwrap().flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("pub") {
                        fs::copy(&path, keys_dst.join(entry.file_name())).unwrap();
                    }
                }
            } else {
                eprintln!(
                    "warning: missing prebuilt/alpine-apk-keys — apk update will show UNTRUSTED signature"
                );
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
        Self::write_passwd(&etc, &dir);
        Self::write_console_configs(&etc, &dir);
        Self::install_ca_certs(&dir);

        // /etc/machine-id — prevents dhcp_vendor "No such file or directory"
        let machine_id = etc.join("machine-id");
        if !machine_id.exists() {
            fs::write(&machine_id, b"eclipseoseclipseoseclipseoseclip\n").unwrap();
        }

        // /etc/hostname
        fs::write(etc.join("hostname"), b"Eclipse\n").unwrap();

        // /etc/fstab — placeholders sustituidos por install-eclipse (sin mount syscall)
        fs::write(
            etc.join("fstab"),
            b"# /etc/fstab - generado por install-eclipse\n\
# <dispositivo>      <punto de montaje>  <tipo>  <opciones>       <dump>  <pass>\n\
__ECLIPSE_ROOT_DEV__  /                  btrfs   defaults          0  1\n\
__ECLIPSE_EFI_DEV___  /boot/efi          vfat    defaults,noatime  0  0\n\
__ECLIPSE_HOME_DEV__  /home              btrfs   defaults          0  0\n\
__ECLIPSE_SWAP_DEV__  none               swap    sw                0  0\n",
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&udhcpc_script, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&udhcpc6_script, fs::Permissions::from_mode(0o755)).unwrap();
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
            let _ = dir::rm(&bin.join("eclipse-resolv"));
            fs::copy(&eclipse_resolv, bin.join("eclipse-resolv")).unwrap();
        }
        let libeclipse_dns = self.libeclipse_dns(&musl);
        if libeclipse_dns.is_file() {
            let _ = dir::rm(&lib.join("libeclipse_dns.so"));
            fs::copy(&libeclipse_dns, lib.join("libeclipse_dns.so")).unwrap();
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

        // 拷贝 resize2fs/e2fsck/mke2fs (e2fsprogs) para el instalador.
        self.install_e2fsprogs_bins(&musl, &bin);
        self.install_thread_tests(&dir);
        // INIT (PID 1): OpenRC by default; busybox init kept as a resilient
        // fallback. busybox lays down `/sbin/init` -> busybox (+ inittab + rcS)
        // first, then OpenRC repoints `/sbin/init` -> `openrc-init` and installs
        // its userland when the (best-effort) cross build succeeds.
        Self::install_base_accounts(&dir);
        self.install_busybox_init(&dir);
        self.install_openrc(&dir, &musl);
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

    /// Lay down the base `/etc/passwd` and `/etc/group` so name lookups resolve.
    ///
    /// OpenRC's `checkpath` sets `/run/lock` to `root:uucp`, so the `uucp` group
    /// must exist or sysinit logs `checkpath: owner 'root:uucp' not found`. This
    /// writes a minimal but standard set of system accounts (including `uucp`,
    /// GID 14, with `root` as a member). `/etc/passwd` is only created when
    /// absent (so it never clobbers accounts added later); `/etc/group` is
    /// created when absent, and otherwise just gets a `uucp` line appended if it
    /// lacks one — idempotent on incremental rebuilds.
    fn install_base_accounts(rootfs: &Path) {
        let etc = rootfs.join("etc");
        let _ = fs::create_dir_all(&etc);

        let passwd = etc.join("passwd");
        if !passwd.exists() {
            fs::write(
                &passwd,
                "root:x:0:0:root:/root:/bin/sh\n\
                 nobody:x:65534:65534:nobody:/:/bin/false\n",
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

    /// Cross-compile libcap (≥2.33) with the musl toolchain into a sysroot and
    /// return it (best-effort, `None` on failure).
    ///
    /// OpenRC's `start-stop-daemon` / `supervise-daemon` `#include
    /// <sys/capability.h>` and call `cap_*` unconditionally on Linux (guarded
    /// only by `#ifdef __linux__`, which the musl cross-compiler defines), and
    /// the meson build hard-requires `dependency('libcap', version: '>=2.33')`
    /// with no option to turn it off. The bare musl sysroot ships no libcap, so
    /// it must be built and exposed (headers + `libcap.pc` + `libcap.a`) before
    /// OpenRC can configure or link. The shared `libcap.so*` is removed after
    /// install so OpenRC's static link resolves `-lcap` to `libcap.a` (OpenRC is
    /// built fully static — see [`openrc`](Self::openrc) — so nothing needs the
    /// shared object at runtime).
    fn libcap(&self, musl: &Path) -> Option<PathBuf> {
        const VERSION: &str = "2.69";
        let sysroot = self.0.target().join("libcap-sysroot");
        if sysroot.join("lib").join("libcap.a").exists() {
            return Some(sysroot);
        }

        // Source: the canonical kernel.org release tarball.
        let src = REPOS.join(format!("libcap-{VERSION}"));
        if !src.join("libcap").join("Makefile").is_file() {
            fs::create_dir_all(&*REPOS).unwrap();
            fs::create_dir_all(self.0.target()).unwrap();
            let tgz = self.0.target().join(format!("libcap-{VERSION}.tar.gz"));
            let url = format!(
                "https://www.kernel.org/pub/linux/libs/security/linux-privs/libcap2/libcap-{VERSION}.tar.gz"
            );
            println!("Downloading libcap {VERSION} for OpenRC...");
            let status = Ext::new("wget").arg("-q").arg("-O").arg(&tgz).arg(&url).status();
            if !status.success() {
                eprintln!("warning: failed to download libcap {VERSION}; OpenRC build skipped");
                return None;
            }
            let status = Ext::new("tar")
                .arg("xzf")
                .arg(&tgz)
                .arg("-C")
                .arg(&*REPOS)
                .status();
            if !status.success() || !src.join("libcap").join("Makefile").is_file() {
                eprintln!("warning: failed to extract libcap {VERSION}; OpenRC build skipped");
                return None;
            }
        }

        let musl = musl.canonicalize().unwrap();
        let arch = self.0.name();
        let bin = musl.join("bin");
        let cc = format!("{}/{}-linux-musl-gcc", bin.display(), arch);
        let ar = format!("{}/{}-linux-musl-ar", bin.display(), arch);
        let ranlib = format!("{}/{}-linux-musl-ranlib", bin.display(), arch);
        let objcopy = format!("{}/{}-linux-musl-objcopy", bin.display(), arch);
        let strip_tool = format!("{}/{}-linux-musl-strip", bin.display(), arch);
        let libcap_dir = src.join("libcap");

        // The cross vars are identical for build and install; `DYNAMIC=yes`
        // builds the shared lib OpenRC links against, `GOLANG=no`/`PAM_CAP=no`
        // skip the bindings/module we don't ship. `BUILD_CC` is the host gcc for
        // the native code-gen helpers.
        let cross_args = move |m: &mut Make| {
            m.arg(format!("CC={cc}"))
                .arg("BUILD_CC=gcc")
                .arg(format!("AR={ar}"))
                .arg(format!("RANLIB={ranlib}"))
                .arg(format!("OBJCOPY={objcopy}"))
                .arg(format!("STRIP={strip_tool}"))
                .arg("GOLANG=no")
                .arg("PAM_CAP=no")
                .arg("DYNAMIC=yes")
                .arg("SHARED=yes")
                .arg("COPTS=-O2")
                .arg("prefix=/")
                .arg("lib=lib");
        };

        dir::rm(&sysroot).unwrap();
        let mut build = Make::new();
        build.current_dir(&libcap_dir);
        cross_args(&mut build);
        if !build.status().success() {
            eprintln!("warning: libcap build failed; OpenRC build skipped");
            return None;
        }
        let mut install = Make::new();
        install.current_dir(&libcap_dir).arg("install");
        cross_args(&mut install);
        install.arg(format!("DESTDIR={}", sysroot.display()));
        if !install.status().success() {
            eprintln!("warning: libcap install failed; OpenRC build skipped");
            return None;
        }
        // Drop the shared objects so OpenRC's `-static` link picks `libcap.a`
        // (and the installed system needs no libcap.so at runtime).
        if let Ok(entries) = fs::read_dir(sysroot.join("lib")) {
            for entry in entries.flatten() {
                let n = entry.file_name();
                let n = n.to_string_lossy();
                if n.starts_with("libcap.so") || n.starts_with("libpsx.so") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        if sysroot.join("lib").join("libcap.a").exists() {
            Some(sysroot)
        } else {
            eprintln!("warning: libcap produced no libcap.a; OpenRC build skipped");
            None
        }
    }

    /// Cross-compile the latest OpenRC with the musl toolchain (best-effort) and
    /// return the staging directory ready to be mirrored into the rootfs, or
    /// `None` if the build is unavailable.
    ///
    /// Mirrors the busybox / e2fsprogs conventions: the source is cloned on first
    /// use, built in a separate tree, and the staged install is cached (a
    /// subsequent build returns immediately once `sbin/openrc-init` is present).
    ///
    /// OpenRC (≥0.54) builds with meson + ninja. The recipe — validated by
    /// cross-building OpenRC 0.63 against a musl-built libcap and running the
    /// resulting binaries — needs four things the naive invocation misses:
    ///   1. libcap (≥2.33) is a HARD dependency on Linux: built by [`libcap`] and
    ///      exposed through `PKG_CONFIG_SYSROOT_DIR` / `PKG_CONFIG_LIBDIR`.
    ///   2. `pkg-config` must be declared in the cross-file `[binaries]`, else
    ///      meson reports "Found pkg-config: NO" and every dependency lookup dies.
    ///   3. `-Dpam=false`: the `pam` option DEFAULTS to true and musl ships no
    ///      libpam, so the default build fails at `cc.find_library('pam')`.
    ///   4. A FULLY STATIC, non-PIE link (`default_library=static`, `-static`,
    ///      `-no-pie`): the binaries come out as static ET_EXEC, exactly like
    ///      busybox / apk / e2fsprogs. This is not just convention — a *dynamic*
    ///      OpenRC (interpreter + librc/libcap in their own sub-VMARs) drove the
    ///      kernel through the `fork`-time sub-VMAR clone path that static
    ///      binaries never exercise, which corrupted memory under the heavy
    ///      fork/exec churn of `openrc sysinit` (one CPU faulted with a mangled
    ///      frame pointer). Static binaries reuse busybox's proven fork path and
    ///      need no librc/libcap/ld-musl at runtime.
    fn openrc(&self, musl: &Path) -> Option<PathBuf> {
        let libcap_sysroot = self.libcap(musl)?;
        let staging = self.0.target().join("openrc-install");
        let init_bin = staging.join("sbin").join("openrc-init");
        if init_bin.is_file() {
            return Some(staging);
        }

        // meson + ninja + pkg-config are required. ninja/pkg-config ship in CI;
        // meson is installed best-effort via pip3 when missing.
        if !host_tool_exists("ninja") || !host_tool_exists("pkg-config") {
            eprintln!(
                "warning: ninja/pkg-config not found; skipping OpenRC build (busybox init remains PID 1)"
            );
            return None;
        }
        if !host_tool_exists("meson") {
            println!("OpenRC: meson not found, attempting `pip3 install --user meson`...");
            let _ = Ext::new("pip3")
                .arg("install")
                .arg("--user")
                .arg("-q")
                .arg("meson")
                .status();
        }
        if !host_tool_exists("meson") {
            eprintln!(
                "warning: meson unavailable; skipping OpenRC build (busybox init remains PID 1)"
            );
            return None;
        }

        // Source (shallow clone of the canonical upstream repo).
        let source = REPOS.join("openrc");
        if !source.is_dir() {
            fetch_online!(source, |tmp| {
                Git::clone("https://github.com/OpenRC/openrc.git")
                    .dir(tmp)
                    .single_branch()
                    .depth(1)
                    .done()
            });
        }

        let musl = musl.canonicalize().unwrap();
        let arch = self.0.name();
        let musl_bin = musl.join("bin");
        let cc = format!("{}/{}-linux-musl-gcc", musl_bin.display(), arch);
        let ar = format!("{}/{}-linux-musl-ar", musl_bin.display(), arch);
        let strip_tool = format!("{}/{}-linux-musl-strip", musl_bin.display(), arch);
        let cpu_family = match self.0 {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
            Arch::Riscv64 => "riscv64",
        };

        fs::create_dir_all(self.0.target()).unwrap();

        // meson cross-file: the musl toolchain, a `pkg-config` so libcap resolves,
        // and `[built-in options]` forcing a static, non-PIE link for every
        // executable (`default_library=static` already drops the shared libs, so
        // `-static -no-pie` is safe globally — no shared-object link to break).
        let cross = self.0.target().join("openrc-cross.ini");
        fs::write(
            &cross,
            format!(
                "[binaries]\n\
                 c = '{cc}'\n\
                 ar = '{ar}'\n\
                 strip = '{strip_tool}'\n\
                 pkg-config = 'pkg-config'\n\
                 \n\
                 [built-in options]\n\
                 c_args = ['-fno-PIE']\n\
                 c_link_args = ['-static', '-no-pie']\n\
                 \n\
                 [host_machine]\n\
                 system = 'linux'\n\
                 cpu_family = '{cpu_family}'\n\
                 cpu = '{arch}'\n\
                 endian = 'little'\n",
            ),
        )
        .unwrap();

        // pkg-config env: resolve ONLY the musl-built libcap, with its absolute
        // (prefix=/) paths rewritten into the sysroot via PKG_CONFIG_SYSROOT_DIR.
        let pkgconfig_libdir = libcap_sysroot.join("lib").join("pkgconfig");

        // Fresh build tree. prefix=/ with a single `/lib` (no `/usr` split, the
        // busybox-style layout the rootfs already uses) and the libexec helpers
        // under `/lib/rc`. `-Dpam=false` + completions off keep it lean and
        // dependency-free on the musl sysroot.
        let build = self.0.target().join("openrc-build");
        dir::rm(&build).unwrap();
        let status = Ext::new("meson")
            .arg("setup")
            .arg(&build)
            .arg(&source)
            .arg(format!("--cross-file={}", cross.display()))
            .arg("--prefix=/")
            .arg("--sysconfdir=/etc")
            .arg("--bindir=bin")
            .arg("--sbindir=sbin")
            .arg("--libdir=lib")
            .arg("--libexecdir=lib")
            .arg("--buildtype=release")
            .arg("-Ddefault_library=static")
            .arg("-Dpam=false")
            .arg("-Dbash-completions=false")
            .arg("-Dzsh-completions=false")
            .env("PKG_CONFIG_SYSROOT_DIR", &libcap_sysroot)
            .env("PKG_CONFIG_LIBDIR", &pkgconfig_libdir)
            .env("PKG_CONFIG_PATH", "")
            .status();
        if !status.success() {
            eprintln!("warning: OpenRC meson setup failed; busybox init remains PID 1");
            return None;
        }
        let status = Ext::new("ninja").arg("-C").arg(&build).status();
        if !status.success() {
            eprintln!("warning: OpenRC ninja build failed; busybox init remains PID 1");
            return None;
        }
        dir::rm(&staging).unwrap();
        fs::create_dir_all(&staging).unwrap();
        let status = Ext::new("ninja")
            .arg("-C")
            .arg(&build)
            .arg("install")
            .env("DESTDIR", &staging)
            .status();
        if !status.success() {
            eprintln!("warning: OpenRC install failed; busybox init remains PID 1");
            return None;
        }
        if init_bin.is_file() {
            println!("Built OpenRC (static) -> {}", staging.display());
            Some(staging)
        } else {
            eprintln!("warning: OpenRC build produced no openrc-init; busybox init remains PID 1");
            None
        }
    }

    /// Install OpenRC as the default init system (PID 1) in `rootfs`
    /// (best-effort); returns `true` on success, `false` if OpenRC is
    /// unavailable and the busybox init fallback should stand.
    ///
    /// The staged `DESTDIR` tree from [`openrc`](Self::openrc) is mirrored into
    /// the rootfs verbatim (the `openrc-init` / `openrc` / `rc-*` binaries into
    /// `/sbin` and `/bin`, the `librc` / `libeinfo` shared libs and the `/lib/rc`
    /// libexec helpers into `/lib`, and the stock service scripts and config into
    /// `/etc/init.d`, `/etc/conf.d`, `/etc/rc.conf`), and `/sbin/init` is
    /// repointed at `openrc-init`. The binaries are fully static, so nothing
    /// extra (librc/libcap/ld-musl) is needed in `/lib` at runtime.
    ///
    /// The active runlevels (`sysinit` / `boot` / `default` / `shutdown`) are
    /// reset to **empty**: the stock install seeds ~30 service symlinks (mount,
    /// hostname, fsck, devfs, …) but the Eclipse kernel already mounts the root
    /// fs, brings up the network and spawns the per-VT shells, so those would
    /// only fight it. The full OpenRC userland is still present, so an operator
    /// can `rc-update add <svc> default` to wire real services later.
    fn install_openrc(&self, rootfs: &Path, musl: &Path) -> bool {
        let staging = match self.openrc(musl) {
            Some(s) => s,
            None => return false,
        };

        // Mirror the staged tree (sbin/bin/lib/etc/…) into the rootfs, preserving
        // symlinks and permissions. Robust to upstream path tweaks.
        copy_tree(&staging, rootfs);

        // /sbin/init -> openrc-init (PID 1). The kernel boots INIT=/sbin/init by
        // default; this makes that resolve to OpenRC, overriding the busybox
        // fallback symlink laid down by `install_busybox_init`.
        let sbin = rootfs.join("sbin");
        let _ = fs::create_dir_all(&sbin);
        let init_link = sbin.join("init");
        let _ = fs::remove_file(&init_link);
        #[cfg(unix)]
        {
            let _ = unix::fs::symlink("openrc-init", &init_link);
        }

        // Minimal runlevels: wipe the ~30 stock service symlinks the install
        // seeded, then recreate the runlevel dirs empty. openrc-init runs through
        // sysinit/boot/default launching nothing (so it never duplicates the
        // kernel's own boot work), then idles as PID 1 reaping orphaned children
        // — the same lean contract as the busybox inittab.
        let runlevels = rootfs.join("etc").join("runlevels");
        let _ = fs::remove_dir_all(&runlevels);
        for rl in ["sysinit", "boot", "default", "shutdown"] {
            let _ = fs::create_dir_all(runlevels.join(rl));
        }

        // Trim the ~40 stock service scripts from /etc/init.d (keep the *.sh
        // helpers and the directory). On Eclipse the kernel already mounts root,
        // brings up the network and spawns the shells, so the stock OS services
        // (sysfs, devfs, hostname, fsck, localmount, network, swap, …) are
        // redundant. More importantly, openrc's first-boot dependency cache
        // ("Caching service dependencies") sources EVERY script in /etc/init.d in
        // its own shell; reaping that burst of ~40 orphaned shells wedged
        // openrc-init before it reached its idle poll loop, so it never settled
        // as PID 1. With init.d trimmed the depscan is trivial. Services can be
        // re-added later (e.g. via apk) and wired with `rc-update add <svc>`.
        let initd = rootfs.join("etc").join("init.d");
        if let Ok(entries) = fs::read_dir(&initd) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // Drop the stock service scripts (no `.sh`) — they fight the
                // kernel's own boot work. Also drop `functions.sh`: it is a zsh
                // compat shim that runs `emulate sh` (a zsh-only builtin) which
                // busybox `ash` cannot parse, so OpenRC's depscan/boot aborts on
                // it. The real helper library lives at `/lib/rc/sh/functions.sh`,
                // which OpenRC sources directly, so removing the `/etc/init.d`
                // copy is harmless on the empty-runlevel Eclipse boot.
                let keep = name.ends_with(".sh") && name != "functions.sh";
                if !keep {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }

        // /etc/rc.conf tuned for Eclipse: keep OpenRC lean and out of the
        // kernel's way (no parallel start, no cgroup management, no syslog
        // dependency). `rc_sys=""` = bare-metal/no-container heuristics.
        fs::write(
            rootfs.join("etc").join("rc.conf"),
            b"# Eclipse OS - OpenRC global config.\n\
              # The kernel already mounts root, configures the network and spawns\n\
              # the per-VT shells, so OpenRC runs as a lean PID 1 supervisor: the\n\
              # default runlevels are empty and you opt services in explicitly\n\
              # with `rc-update add <service> default`.\n\
              rc_sys=\"\"\n\
              rc_parallel=\"NO\"\n\
              rc_cgroup_mode=\"none\"\n\
              rc_logger=\"NO\"\n\
              unicode=\"YES\"\n",
        )
        .unwrap();

        // /run/openrc is created on the runtime tmpfs; ensure the mount point and
        // /run exist so OpenRC's state dir has somewhere to attach.
        let _ = fs::create_dir_all(rootfs.join("run").join("openrc"));

        // Stamp the dependency-tree source files (/etc/init.d, /etc/conf.d,
        // /etc/rc.conf) with a fixed PAST mtime (2020-01-01 UTC). OpenRC's
        // `rc_deptree_update_needed` takes the newest mtime among these and, if
        // it is greater than the current time, prints "clock skew detected" and
        // rewrites the deptree. The files otherwise carry their build-host mtime,
        // which can sit *ahead* of the guest/RTC clock (timezone / RTC-convention
        // differences), triggering the spurious warning on every boot. Pinning
        // them to a clearly-past instant makes them older than any sane boot
        // clock, on QEMU and on real hardware alike.
        Self::pin_mtime_past(&rootfs.join("etc").join("init.d"));
        Self::pin_mtime_past(&rootfs.join("etc").join("conf.d"));
        Self::pin_mtime_past(&rootfs.join("etc").join("rc.conf"));

        println!("Installed OpenRC as the default init system (PID 1).");
        true
    }

    /// Set `path` (and, recursively, everything under it if it is a directory)
    /// to a fixed past modification time: 2020-01-01 00:00:00 UTC
    /// (`@1577836800`). Best-effort via `touch`; a failure just leaves the
    /// build-host mtime and at worst re-arms the cosmetic OpenRC clock-skew
    /// warning, so it never aborts the build.
    fn pin_mtime_past(path: &Path) {
        if !path.exists() {
            return;
        }
        // `@SECONDS` is an absolute, timezone-independent epoch; `find ... +`
        // touches the path itself and every entry beneath it in one pass.
        let _ = Ext::new("find")
            .arg(path)
            .arg("-exec")
            .arg("touch")
            .arg("-h")
            .arg("-d")
            .arg("@1577836800")
            .arg("{}")
            .arg("+")
            .status();
    }

    /// Compila thr3 con gcc del host (bare metal, sin libc).
    fn thread_test_thr3(&self) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("thread-tests");
        let source = dir.join("thr3.c");
        let executable = dir.join("thr3-metal");
        if executable.is_file() && source.is_file() {
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

    fn write_profile(etc: &Path) {
        fs::write(
            etc.join("profile"),
            b"export PATH=/bin:/sbin:/usr/bin:/usr/sbin\n\
              export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt\n\
              export SSL_CERT_DIR=/etc/ssl/certs\n\
              export CURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt\n\
              export LD_PRELOAD=/lib/libeclipse_dns.so\n\
              export HOME=/root\n\
              export TERM=xterm-256color\n\
              # No GPU here: wlroots/labwc must use the software (pixman) renderer.\n\
              # Otherwise wlroots tries GLES2/EGL then Vulkan, both fail, and labwc\n\
              # exits with 'Could not initialize renderer' (it does not auto-fall\n\
              # back to pixman). See docs/README-drm.md.\n\
              export WLR_RENDERER=pixman\n\
              export WLR_RENDERER_ALLOW_SOFTWARE=1\n\
              # Runtime dir for the Wayland socket (created on demand, mode 0700).\n\
              export XDG_RUNTIME_DIR=/run/user/0\n\
              [ -d \"$XDG_RUNTIME_DIR\" ] || { mkdir -p \"$XDG_RUNTIME_DIR\" && chmod 0700 \"$XDG_RUNTIME_DIR\"; }\n",
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
                  nobody:x:65534:65534:nobody:/:/sbin/nologin\n",
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

    /// Creates symlinks in `bin/` for every busybox applet.
    ///
    /// Called on both full and incremental builds so that a rootfs directory
    /// created before this feature existed (or after a partial build) still ends
    /// up with all applet symlinks in the final ext2 image.  Existing entries
    /// (real binaries like `nl_dump`) are never overwritten.
    fn ensure_busybox_applets(bin: &Path) {
        // Base list of essential applets
        let mut applets: Vec<String> = vec![
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
        ]
        .into_iter()
        .map(String::from)
        .collect();

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
        PROJECT_DIR.join("rootfs").join(self.0.name())
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
                Git::clone("https://git.busybox.net/busybox.git")
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
                  s/^# CONFIG_FEATURE_USE_INITTAB is not set$/CONFIG_FEATURE_USE_INITTAB=y/",
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
        const CHIMERA_APK_BASE: &str = "https://repo.chimera-linux.org/apk/latest";

        let arch = self.0.name(); // "x86_64", "aarch64", "riscv64"
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
        if executable.is_file() && source.is_file() {
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
        if executable.is_file() && source.is_file() {
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

    /// Build libeclipse_dns.so (LD_PRELOAD resolver shim).
    fn libeclipse_dns(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("eclipse-resolv");
        let lib = dir.join("libeclipse_dns.so");
        let source = dir.join("resolv.c");
        if lib.is_file() && source.is_file() {
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

    /// Build eclipse-resolv CLI (static).
    fn eclipse_resolv(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("eclipse-resolv");
        let executable = dir.join("eclipse-resolv");
        let source = dir.join("eclipse-resolv.c");
        if executable.is_file() && source.is_file() {
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
        if executable.is_file() && source.is_file() {
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
        if executable.is_file() && source.is_file() {
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
        if executable.is_file() && source.is_file() {
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

    fn strip(&self, musl: impl AsRef<Path>) -> PathBuf {
        musl.as_ref()
            .join("bin")
            .join(format!("{}-linux-musl-strip", self.0.name()))
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
                        dir::rm(&target).unwrap();
                        // `fs::copy` 会拷贝文件内容
                        unix::fs::symlink(source.read_link().unwrap(), target).unwrap();
                    }
                } else if name != musl_libc_ignored {
                    dir::rm(&target).unwrap();
                    fs::copy(source, &target).unwrap();
                    Ext::new(&strip).arg("-s").arg(target).status();
                }
            });
    }
}

/// True if a host build tool can be executed (probed via `--version`). Used to
/// gate the best-effort OpenRC build on meson / ninja being installed.
fn host_tool_exists(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Recursively copy `src` into `dst`, **merging** into existing directories and
/// preserving symlinks and unix permissions. Used to mirror the staged OpenRC
/// `DESTDIR` tree into the rootfs without clobbering sibling files already
/// present there (unlike `dircpy`, which expects a fresh destination).
fn copy_tree(src: &Path, dst: &Path) {
    let md = match fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(_) => return,
    };
    if md.file_type().is_symlink() {
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let target = fs::read_link(src).unwrap();
        let _ = fs::remove_file(dst);
        #[cfg(unix)]
        let _ = unix::fs::symlink(target, dst);
        return;
    }
    if md.is_dir() {
        let _ = fs::create_dir_all(dst);
        for entry in fs::read_dir(src).unwrap().flatten() {
            copy_tree(&entry.path(), &dst.join(entry.file_name()));
        }
        return;
    }
    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::remove_file(dst);
    fs::copy(src, dst).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = fs::metadata(src) {
            let _ = fs::set_permissions(dst, fs::Permissions::from_mode(m.permissions().mode()));
        }
    }
}

/// 为 PATH 环境变量附加路径。
fn join_path_env<I, S>(paths: I) -> OsString
where
    I: IntoIterator<Item = S>,
    S: AsRef<Path>,
{
    let mut path = OsString::new();
    let mut first = true;
    if let Ok(current) = env::var("PATH") {
        path.push(current);
        first = false;
    }
    for item in paths {
        if first {
            first = false;
        } else {
            path.push(":");
        }
        path.push(item.as_ref().canonicalize().unwrap().as_os_str());
    }
    path
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
    !seg.any(|it| !it.chars().all(|ch| ch.is_ascii_digit()))
}
