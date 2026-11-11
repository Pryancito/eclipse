//! The network at boot and its settings window.
//!
//! `eclipse-network-up` is what the `network` service runs (ntpd is ordered
//! after that name). It hands over to `lunarnet --service` when
//! the image has it: that applies `/etc/eclipse/network` (static addresses,
//! routes, DNS servers, IPv4 off) and runs `udhcpc` / `udhcpc6` for the
//! interfaces that want DHCP. Without it, DHCPv4 on the first interface, as
//! before. `lunarnet` itself is the window that edits the file ("Red" in the
//! launcher).

use std::fs;
use std::path::{Path, PathBuf};

use super::LinuxRootfs;
use crate::PROJECT_DIR;
use os_xtask_utils::{CommandExt, Ext};

/// `/usr/local/bin/eclipse-network-up`.
///
/// Named after what it brings up, not after the client it falls back to. It
/// was `eclipse-udhcpc`, run by a service called `udhcpc`, long after lunarnet
/// took the job over --- and a name that says "the old bare DHCP client" is a
/// name that gets deleted as dead weight, which takes the whole network with
/// it: without this service nothing applies `/etc/eclipse/network`, nothing
/// runs a DHCP client, and the NIC sits there with no address.
pub(super) fn write_network_wrapper(localbin: &Path) {
    // The old name, left behind by an incremental rootfs, is dead weight that
    // still looks runnable.
    let _ = fs::remove_file(localbin.join("eclipse-udhcpc"));
    fs::write(
        localbin.join("eclipse-network-up"),
        b"#!/bin/sh\n\
          # Eclipse OS: the network at boot, in the foreground for eclipse-init.\n\
          # lunarnet --service applies /etc/eclipse/network and supervises the\n\
          # DHCP clients; SIGHUP (lunarnet's Aplicar) makes it read it again.\n\
          for d in /usr/local/bin /usr/bin /bin; do\n\
          \x20 [ -x \"$d/lunarnet\" ] && exec \"$d/lunarnet\" --service\n\
          done\n\
          # Without it: foreground DHCPv4 on the first real interface; init\n\
          # respawns the client if it dies.\n\
          command -v udhcpc >/dev/null 2>&1 || { echo 'eclipse-network-up: udhcpc not found' >&2; sleep 5; exit 127; }\n\
          # -s is NOT optional: busybox udhcpc's compiled-in default script\n\
          # path is not where Eclipse stages the script, so without -s the\n\
          # lease is obtained but never APPLIED -- the address, resolv.conf\n\
          # and route are all set by this script on the 'bound' event.\n\
          SCRIPTv4=/usr/share/udhcpc/default.script\n\
          [ -x \"$SCRIPTv4\" ] || SCRIPTv4=/etc/udhcpc/default.script\n\
          for i in $(ip -o link show 2>/dev/null | sed 's/^[0-9]*: //; s/[@:].*//' | grep -v '^lo'); do\n\
          \x20 exec udhcpc -i \"$i\" -f -R -s \"$SCRIPTv4\"\n\
          done\n\
          # `ip` may not have listed anything yet (netlink dump raced the\n\
          # NIC probe). eth0 is the sequential name the kernel assigns to\n\
          # the first Ethernet NIC.\n\
          echo 'eclipse-network-up: no interface from ip link; trying eth0' >&2\n\
          exec udhcpc -i eth0 -f -R -s \"$SCRIPTv4\"\n",
    )
    .unwrap();
}

/// The `unet` service and `/usr/local/bin/eclipse-unet`, which it runs: the
/// driver of the first virtio network card, as a process.
///
/// `udriver =` keeps the service out of every boot where the kernel drives
/// its own cards, which is all of them unless the command line says
/// `udriver=virtio-net@0`. Supervised: when the driver dies the kernel resets
/// the card and keeps the interface, addresses and connections included, for
/// the next one, which init starts at once -- so `network` is ordered after
/// it only for the first start, and has nothing to do on any later one.
///
/// `service` is the driver doing its job and nothing else: it hands the card
/// to the kernel as an interface and carries its frames until it is killed.
pub(super) fn write_unet(localbin: &Path, svc_dir: &Path) {
    fs::write(
        svc_dir.join("unet.service"),
        b"# Driver of the network card left to userspace (boot with\n\
          # udriver=virtio-net@0). See /usr/local/bin/eclipse-unet.\n\
          exec = /usr/local/bin/eclipse-unet\n\
          type = respawn\n\
          udriver = virtio-net@0\n",
    )
    .unwrap();
    fs::write(
        localbin.join("eclipse-unet"),
        b"#!/bin/sh\n\
          # Eclipse OS: the userspace driver of the network card the kernel\n\
          # left alone (udriver=virtio-net@0), in the foreground for\n\
          # eclipse-init, which starts it again if it dies. The kernel keeps\n\
          # the interface meanwhile. See tools/zx-virtio-net.\n\
          [ -x /bin/zx-virtio-net ] || { echo 'eclipse-unet: /bin/zx-virtio-net is not in this image' >&2; sleep 5; exit 127; }\n\
          exec /bin/zx-virtio-net service 0\n",
    )
    .unwrap();
}

impl LinuxRootfs {
    /// Put the driver `unet` runs in the image's `/bin`, built for its
    /// architecture.
    pub(super) fn install_zx_virtio_net(&self, musl: &Path, bin: &Path) {
        let built = self.zx_virtio_net(musl);
        if built.is_file() {
            let dst = bin.join("zx-virtio-net");
            let _ = fs::remove_file(&dst);
            let _ = fs::copy(&built, &dst);
        }
    }

    /// Compile zx-virtio-net (`tools/zx-virtio-net`): the driver, in
    /// userspace, of a virtio network card the kernel was told to leave
    /// alone (`udriver=virtio-net@N`).
    fn zx_virtio_net(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("zx-virtio-net");
        let executable = dir.join("zx-virtio-net");
        let source = dir.join("zx-virtio-net.c");
        // Rebuild if missing, if the source is newer, or if the binary there
        // is another architecture's.
        if executable.is_file() && source.is_file() && self.cached_for_target(&executable) {
            let newer = |a: &Path, b: &Path| {
                let at = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
                matches!((at(a), at(b)), (Some(a), Some(b)) if a >= b)
            };
            if newer(&executable, &source) {
                return executable;
            }
        }

        println!("Compiling zx-virtio-net...");
        let musl = musl.canonicalize().unwrap();
        let cc = musl
            .join("bin")
            .join(format!("{}-linux-musl-gcc", self.0.name()));
        let strip = self.strip(&musl);

        // A build that fails must not leave the last one, for whatever
        // architecture it was, to be copied in its place.
        let _ = fs::remove_file(&executable);
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-pthread")
            .arg("-O2")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile zx-virtio-net");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }
}

/// "Red" in the launcher: `eclipse-network`, and its `.desktop` entry.
///
/// Through a wrapper like every other entry: when the image has no lunarnet
/// the row still opens something useful, a terminal on what `ip` says.
pub(super) fn write_network_desktop_entry(rootfs: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let localbin = rootfs.join("usr/local/bin");
    let _ = fs::create_dir_all(&localbin);
    let wrapper = localbin.join("eclipse-network");
    fs::write(
        &wrapper,
        b"#!/bin/sh\n\
          # Eclipse OS: the network settings (IPv4, IPv6, DNS per interface).\n\
          # See tools/lunarbar/src/bin/lunarnet.rs.\n\
          if command -v lunarnet >/dev/null 2>&1; then\n\
          \x20 exec lunarnet \"$@\"\n\
          fi\n\
          exec /usr/local/bin/eclipse-terminal sh -c \"ip addr; echo; ip route; ip -6 route; echo; echo '[enter]'; read _\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();

    let dir = rootfs.join("root/.local/share/applications");
    let _ = fs::create_dir_all(&dir);
    fs::write(
        dir.join("lunarnet.desktop"),
        "[Desktop Entry]\n\
         Version=1.0\n\
         Type=Application\n\
         Name=Red\n\
         GenericName=Network Settings\n\
         Comment=IPv4, IPv6 y DNS de cada interfaz: DHCP, automática o manual\n\
         Exec=/usr/local/bin/eclipse-network\n\
         TryExec=/usr/local/bin/eclipse-network\n\
         Terminal=false\n\
         Icon=preferences-system-network\n\
         Categories=System;Settings;Network;\n\
         Keywords=red;network;ipv4;ipv6;dns;dhcp;ip;wifi;ethernet;\n\
         StartupNotify=false\n",
    )
    .unwrap();
}
