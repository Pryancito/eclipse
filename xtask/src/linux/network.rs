//! The network at boot and its settings window.
//!
//! `eclipse-udhcpc` is what the `udhcpc` service runs, as it always was (ntpd
//! is ordered after that name). It hands over to `lunarnet --service` when
//! the image has it: that applies `/etc/eclipse/network` (static addresses,
//! routes, DNS servers, IPv4 off) and runs `udhcpc` / `udhcpc6` for the
//! interfaces that want DHCP. Without it, DHCPv4 on the first interface, as
//! before. `lunarnet` itself is the window that edits the file ("Red" in the
//! launcher).

use std::fs;
use std::path::Path;

/// `/usr/local/bin/eclipse-udhcpc`.
pub(super) fn write_udhcpc_wrapper(localbin: &Path) {
    fs::write(
        localbin.join("eclipse-udhcpc"),
        b"#!/bin/sh\n\
          # Eclipse OS: the network at boot, in the foreground for eclipse-init.\n\
          # lunarnet --service applies /etc/eclipse/network and supervises the\n\
          # DHCP clients; SIGHUP (lunarnet's Aplicar) makes it read it again.\n\
          for d in /usr/local/bin /usr/bin /bin; do\n\
          \x20 [ -x \"$d/lunarnet\" ] && exec \"$d/lunarnet\" --service\n\
          done\n\
          # Without it: foreground DHCPv4 on the first real interface; init\n\
          # respawns the client if it dies.\n\
          command -v udhcpc >/dev/null 2>&1 || { echo 'eclipse-udhcpc: udhcpc not found' >&2; sleep 5; exit 127; }\n\
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
          echo 'eclipse-udhcpc: no interface from ip link; trying eth0' >&2\n\
          exec udhcpc -i eth0 -f -R -s \"$SCRIPTv4\"\n",
    )
    .unwrap();
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
