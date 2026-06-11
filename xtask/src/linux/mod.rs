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
            // partial build) would produce an ext2 image where `ls`, `cat`, etc. are
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
            // resize2fs/e2fsck/mke2fs (para expandir ROOT y formatear HOME).
            self.install_e2fsprogs_bins(&musl, &bin);
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
                "http://dl-cdn.alpinelinux.org/alpine/edge/main\nhttp://dl-cdn.alpinelinux.org/alpine/edge/community\n",
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
        Self::write_profile(&etc);
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
__ECLIPSE_ROOT_DEV__  /                  ext2    defaults          0  1\n\
__ECLIPSE_EFI_DEV___  /boot/efi          vfat    defaults,noatime  0  0\n\
__ECLIPSE_HOME_DEV__  /home              ext2    defaults          0  0\n\
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
        fs::write(&udhcpc_script,
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
              exit 0\n"
        ).unwrap();
        let udhcpc6_script = udhcpc_dir.join("default6.script");
        fs::write(&udhcpc6_script,
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
              exit 0\n"
        ).unwrap();
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
        fs::write(&openssl_script,
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
              fi\n"
        ).unwrap();
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

        // 拷贝 resize2fs/e2fsck/mke2fs (e2fsprogs) para el instalador.
        self.install_e2fsprogs_bins(&musl, &bin);
    }

    fn write_resolv_conf(etc: &Path) {
        fs::write(
            etc.join("resolv.conf"),
            "nameserver 8.8.8.8\nnameserver 1.1.1.1\n",
        )
        .unwrap();
    }

    fn write_profile(etc: &Path) {
        fs::write(
            etc.join("profile"),
            b"export PATH=/bin:/sbin:/usr/bin:/usr/sbin\n\
              export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt\n\
              export SSL_CERT_DIR=/etc/ssl/certs\n\
              export CURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt\n",
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
            "cat", "cp", "echo", "false", "grep", "gzip", "ip", "kill",
            "ln", "ls", "mkdir", "mv", "pidof", "ping", "ps", "pwd", "rm",
            "rmdir", "sh", "sleep", "stat", "tar", "touch", "true", "uname",
            "usleep", "watch", "ifconfig", "route", "udhcpc", "udhcpc6",
            "sed", "awk", "cmp", "diff", "logger", "hostname", "cut", "sort",
            "uniq", "head", "tail", "wc", "xargs", "find", "test", "expr",
            "id", "date", "env", "chmod", "chown", "vi", "top", "less",
            "ssl_client", "ssl_server", "wget", "traceroute", "traceroute6",
        ].into_iter().map(String::from).collect();

        // Complement the list with `busybox --list` when it runs on the host.
        let busybox_bin = bin.join("busybox");
        if let Ok(out) = std::process::Command::new(&busybox_bin).arg("--list").output() {
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
            .arg("s/.*CONFIG_STATIC.*/CONFIG_STATIC=y/;\
                  s/.*CONFIG_PIE.*/CONFIG_PIE=n/;\
                  s/.*CONFIG_FEATURE_INDIVIDUAL.*/CONFIG_FEATURE_INDIVIDUAL=n/;\
                  s/.*CONFIG_FEATURE_SHARED_BUSYBOX.*/CONFIG_FEATURE_SHARED_BUSYBOX=n/;\
                  s/.*CONFIG_FEATURE_WGET_OPENSSL.*/CONFIG_FEATURE_WGET_OPENSSL=n/;\
                  s/.*CONFIG_FEATURE_WGET_HTTPS.*/CONFIG_FEATURE_WGET_HTTPS=y/;\
                  s/.*CONFIG_SSL_CLIENT.*/CONFIG_SSL_CLIENT=y/;\
                  s/.*CONFIG_FEATURE_IPV6.*/CONFIG_FEATURE_IPV6=y/;\
                  s/.*CONFIG_UDHCPC6.*/CONFIG_UDHCPC6=y/;\
                  s/.*CONFIG_FEATURE_UDHCPC6_RFC3646.*/CONFIG_FEATURE_UDHCPC6_RFC3646=y/")
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
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified())
                {
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
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified())
                {
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

    /// 编译 install-eclipse (static installer for Eclipse OS).
    fn install_eclipse(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("install-eclipse");
        let executable = dir.join("install-eclipse");
        let source = dir.join("install-eclipse.c");
        // Rebuild if missing or if source is newer than the binary.
        if executable.is_file() && source.is_file() {
            if let (Ok(bin_meta), Ok(src_meta)) = (fs::metadata(&executable), fs::metadata(&source))
            {
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified())
                {
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
                if let (Ok(bin_mtime), Ok(src_mtime)) = (bin_meta.modified(), src_meta.modified())
                {
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

    fn strip(&self, musl: impl AsRef<Path>) -> PathBuf {
        musl.as_ref()
            .join("bin")
            .join(format!("{}-linux-musl-strip", self.0.name()))
    }

    /// Cross-compila e2fsprogs (resize2fs, e2fsck, mke2fs) estático con musl y
    /// devuelve el directorio con los binarios ya recortados.
    ///
    /// Necesario porque busybox no incluye estas herramientas: el instalador usa
    /// `resize2fs` (y `e2fsck -f`) para expandir ROOT a toda la partición tras
    /// volcar la imagen, y `mke2fs` para formatear HOME en el layout avanzado.
    ///
    /// Es best-effort: si la descarga o la compilación fallan, se devuelve el
    /// directorio aunque esté vacío/incompleto y los llamantes omiten los
    /// binarios ausentes (igual que el resto de herramientas opcionales).
    fn e2fsprogs(&self, musl: &Path) -> PathBuf {
        let out = self.0.target().join("e2fsprogs");
        let needed = ["resize2fs", "e2fsck", "mke2fs"];
        if needed.iter().all(|n| out.join(n).is_file()) {
            return out;
        }

        // Fuente (clon superficial del mirror canónico de tytso).
        let source = REPOS.join("e2fsprogs");
        if !source.is_dir() {
            fetch_online!(source, |tmp| {
                Git::clone("https://github.com/tytso/e2fsprogs.git")
                    .dir(tmp)
                    .single_branch()
                    .depth(1)
                    .done()
            });
        }

        let musl = musl.canonicalize().unwrap();
        let arch = self.0.name();
        let musl_bin = musl.join("bin");
        let path_env = join_path_env([&musl_bin]);
        let cc = format!("{}/{}-linux-musl-gcc", musl_bin.display(), arch);
        let ar = format!("{}/{}-linux-musl-ar", musl_bin.display(), arch);
        let ranlib = format!("{}/{}-linux-musl-ranlib", musl_bin.display(), arch);
        let strip_tool = format!("{}/{}-linux-musl-strip", musl_bin.display(), arch);

        // Build VPATH en árbol separado para no ensuciar la fuente.
        let build = self.0.target().join("e2fsprogs-build");
        dir::rm(&build).unwrap();
        fs::create_dir_all(&build).unwrap();

        // configure cruzado y estático. Las asignaciones CC=/CFLAGS=/LDFLAGS= se
        // pasan como argumentos (autoconf las acepta así).
        let configure = source.join("configure");
        let status = Ext::new("sh")
            .current_dir(&build)
            .env("PATH", &path_env)
            .arg(configure.display().to_string())
            .arg(format!("CC={cc}"))
            .arg(format!("AR={ar}"))
            .arg(format!("RANLIB={ranlib}"))
            .arg(format!("STRIP={strip_tool}"))
            .arg("CFLAGS=-O2 -fno-PIC -fno-PIE")
            .arg("EXTRA_CFLAGS=-O2 -fno-PIC -fno-PIE")
            .arg("LDFLAGS=-static -no-pie")
            .arg("EXTRA_LDFLAGS=-static -no-pie")
            .arg(format!("--host={arch}-linux-musl"))
            .arg("--disable-nls")
            .arg("--disable-rpath")
            .arg("--disable-defrag")
            .arg("--disable-fuse2fs")
            .arg("--disable-uuidd")
            .status();
        if !status.success() {
            println!("Failed to configure e2fsprogs");
            return out;
        }

        // lib/uuid/Makefile.in enlaza tst_uuid en `all::`; con GCC reciente y
        // -static falla (R_X86_64_32 vs PIE). Solo necesitamos libuuid.a.
        let uuid_mk = build.join("lib/uuid/Makefile");
        if let Ok(text) = fs::read_to_string(&uuid_mk) {
            let patched = text.replace("all:: tst_uuid uuid_time", "all:: uuid_time");
            if patched != text {
                let _ = fs::write(&uuid_mk, patched);
            }
        }

        // Bibliotecas estáticas primero. Los binarios se construyen dentro de cada
        // subdirectorio: `make resize/resize2fs` desde la raíz no enlaza libext2fs.
        let _ = Make::new()
            .current_dir(&build)
            .env("PATH", &path_env)
            .arg("libs")
            .status();
        for (subdir, prog) in [("resize", "resize2fs"), ("e2fsck", "e2fsck"), ("misc", "mke2fs")] {
            let _ = Make::new()
                .current_dir(build.join(subdir))
                .env("PATH", &path_env)
                .arg(prog)
                .status();
        }

        fs::create_dir_all(&out).unwrap();
        let strip = self.strip(&musl);
        for (rel, name) in [
            ("resize/resize2fs", "resize2fs"),
            ("e2fsck/e2fsck", "e2fsck"),
            ("misc/mke2fs", "mke2fs"),
        ] {
            let built = build.join(rel);
            if built.is_file() {
                let dst = out.join(name);
                let _ = dir::rm(&dst);
                if fs::copy(&built, &dst).is_ok() {
                    Ext::new(&strip).arg("-s").arg(&dst).status();
                }
            } else {
                println!("warning: e2fsprogs build did not produce {name}");
            }
        }
        out
    }

    /// Construye e2fsprogs y copia resize2fs/e2fsck/mke2fs en `bin/` (best-effort).
    fn install_e2fsprogs_bins(&self, musl: &Path, bin: &Path) {
        let out = self.e2fsprogs(musl);
        for name in ["resize2fs", "e2fsck", "mke2fs"] {
            let built = out.join(name);
            if built.is_file() {
                let dst = bin.join(name);
                let _ = dir::rm(&dst);
                let _ = fs::copy(&built, &dst);
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
