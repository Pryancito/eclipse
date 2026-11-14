//! The drivers that run as processes, and the boot services that run them.
//!
//! A device the kernel was told to leave alone (`udriver=<kind>@<N>` on its
//! command line) is driven by a program in the image. Each is a boot
//! service -- `unet`, `ublk` -- that `eclipse-init` starts only on a boot
//! that left it a device (the `udriver =` key), watches, and starts again
//! when it dies. The kernel keeps what the driver gave it meanwhile: the
//! interface with its addresses and connections, the disk with what is
//! mounted on it.
//!
//! The drivers the system runs are Rust, the binaries of
//! `tools/eclipse-udrivers`: `udrv-virtio-blk` for a virtio disk and
//! `udrv-virtio-net` for a virtio network card. The C programs in
//! `tools/zx-virtio-blk` and `tools/zx-virtio-net` are the tools that check
//! and time a driver; each also carries a driver of its own (`service`),
//! which its tests start to compare against (`bench ... c`) and which no
//! boot service runs: an image whose Rust build failed has no driver for
//! a device left to userspace, and the service's wrapper says so.
//!
//! `eclipse-netstack` (`tools/eclipse-netstack`) is the network stack as a
//! process, and what the `unet` service runs on a boot that leaves it the
//! card (`udriver=virtio-net@0`): it is the card's driver itself, takes
//! the card's frames instead of the kernel and asks a DHCP server for its
//! address, or has the one the boot gives it (`unetstack=ADDR/PREFIX`).
//! With `unetdriver=process` as well it runs `udrv-virtio-net` for the
//! card, a process of its own. `unetstack=kernel` is the boot that keeps
//! the kernel's stack: there the service runs the driver alone, which
//! hands the card to the kernel. Whose sockets that stack's are is the
//! kernel's to say (`linux_object::net::user_stack::boot`).

use std::fs;
use std::path::{Path, PathBuf};

use super::mtime::newest_mtime_under;
use super::LinuxRootfs;
use crate::commands::optional_status;
use crate::PROJECT_DIR;
use os_xtask_utils::{CommandExt, Ext};

/// The C tools: the directory under `tools/`, which is also the binary's
/// name.
const TOOLS: [&str; 2] = ["zx-virtio-net", "zx-virtio-blk"];

/// The Rust drivers' package, under `tools/`, and its binaries.
const DRIVERS_PACKAGE: &str = "eclipse-udrivers";
const DRIVERS: [&str; 2] = ["udrv-virtio-blk", "udrv-virtio-net"];

/// The network stack that runs as a process: its package, under `tools/`,
/// its binaries -- the stack, what runs a program on it, and the UDP and
/// TCP echo servers and the TCP client it is measured with -- and what it
/// is built from besides its own sources (the drivers' library and the
/// tree's smoltcp, both by path).
const NETSTACK_PACKAGE: &str = "eclipse-netstack";
const NETSTACK: [&str; 5] = [
    "eclipse-netstack",
    "eclipse-netstack-use",
    "eclipse-udp-echo",
    "eclipse-tcp-echo",
    "eclipse-tcp-bench",
];
const NETSTACK_USES: [&str; 2] = ["tools/eclipse-udrivers/src", "smoltcp/src"];

/// The `unet` and `ublk` services and the wrappers they run, and
/// `ublk-mounts`, which mounts what fstab puts on the served disk.
///
/// `udriver =` keeps each out of every boot where the kernel drives the
/// device itself, which is all of them unless the command line says
/// otherwise. Supervised: when a driver dies the kernel resets its device
/// and init starts the next one at once.
///
/// `service` is a driver doing its job and nothing else: it hands the
/// device to the kernel -- as an interface, as a disk -- and serves it
/// until it is killed.
pub(super) fn write_services(localbin: &Path, svc_dir: &Path) {
    // The card: the first one, which is what `network` is ordered after.
    fs::write(
        svc_dir.join("unet.service"),
        b"# The network card left to userspace (boot with\n\
          # udriver=virtio-net@0): the network stack that runs as a process,\n\
          # or the card's driver alone. See /usr/local/bin/eclipse-unet.\n\
          exec = /usr/local/bin/eclipse-unet\n\
          type = respawn\n\
          udriver = virtio-net@0\n",
    )
    .unwrap();
    fs::write(
        localbin.join("eclipse-unet"),
        b"#!/bin/sh\n\
          # Eclipse OS: what drives the network card the kernel left alone\n\
          # (udriver=virtio-net@0), in the foreground for eclipse-init,\n\
          # which starts it again if it dies. See tools/eclipse-udrivers.\n\
          # The driver is the Rust one and only that one: the C tool's own\n\
          # driver is for checking and timing, never the system's.\n\
          [ -x /bin/udrv-virtio-net ] || { echo 'eclipse-unet: /bin/udrv-virtio-net is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }\n\
          # The card does not go to the kernel: the network stack that runs\n\
          # as a process takes it and is its driver too. See\n\
          # tools/eclipse-netstack. It asks a DHCP server for its address\n\
          # and gateway (unetstack=dhcp on the kernel command line, or no\n\
          # such word); unetstack=ADDR/PREFIX gives it that address, and\n\
          # unetstack=ADDR/PREFIX,GATEWAY a gateway too.\n\
          # unetstack=kernel: the card is for the kernel's own stack, and\n\
          # what runs here is its driver alone.\n\
          words=$(tr ': ' '\\n\\n' < /proc/cmdline)\n\
          stack=$(echo \"$words\" | sed -n 's/^unetstack=//p' | head -n 1)\n\
          [ -n \"$stack\" ] || stack=dhcp\n\
          if [ \"$stack\" != kernel ]; then\n\
          \x20   [ -x /bin/eclipse-netstack ] || { echo 'eclipse-unet: /bin/eclipse-netstack is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }\n\
          \x20   # unetdriver=process: the driver in a process of its own, which\n\
          \x20   # the stack starts and starts again, as it was before the stack\n\
          \x20   # drove the card itself. A driver that dies then costs no\n\
          \x20   # connection, and every frame a change of process each way.\n\
          \x20   case $(echo \"$words\" | sed -n 's/^unetdriver=//p' | head -n 1) in\n\
          \x20       process) set -- driver=process ;;\n\
          \x20       *) set -- ;;\n\
          \x20   esac\n\
          \x20   exec /bin/eclipse-netstack 0 \"$stack\" \"$@\"\n\
          fi\n\
          exec /bin/udrv-virtio-net 0\n",
    )
    .unwrap();

    // The disk: whichever the command line names. Which virtio disk is
    // spare depends on the machine (the root may be the first), so the
    // service is gated on the kind and the wrapper reads the number.
    fs::write(
        svc_dir.join("ublk.service"),
        b"# Driver of the virtio disk left to userspace (boot with\n\
          # udriver=virtio-blk@N). See /usr/local/bin/eclipse-ublk.\n\
          exec = /usr/local/bin/eclipse-ublk\n\
          type = respawn\n\
          udriver = virtio-blk\n",
    )
    .unwrap();
    fs::write(
        localbin.join("eclipse-ublk"),
        b"#!/bin/sh\n\
          # Eclipse OS: the userspace driver of the virtio disk the kernel\n\
          # left alone (udriver=virtio-blk@N), in the foreground for\n\
          # eclipse-init, which starts it again if it dies. The kernel keeps\n\
          # the disk meanwhile, with what is mounted on it, and what was being\n\
          # read or written goes on with the next driver. See\n\
          # tools/eclipse-udrivers.\n\
          # Which disk: the first virtio-blk@N in the kernel's udriver= list.\n\
          n=$(tr ': ' '\\n\\n' < /proc/cmdline | sed -n 's/^udriver=//p' | tr ',' '\\n' | sed -n 's/^virtio-blk@//p' | head -n 1)\n\
          [ -n \"$n\" ] || { echo 'eclipse-ublk: no virtio-blk@N in udriver= on the kernel command line' >&2; sleep 5; exit 1; }\n\
          # The driver is the Rust one and only that one: the C tool's own\n\
          # driver is for checking and timing, never the system's.\n\
          [ -x /bin/udrv-virtio-blk ] || { echo 'eclipse-ublk: /bin/udrv-virtio-blk is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }\n\
          exec /bin/udrv-virtio-blk \"$n\"\n",
    )
    .unwrap();

    // What `/etc/fstab` puts on that disk. The kernel goes through fstab
    // once, as init starts, and a disk whose driver is a service is not
    // there yet: its lines fail there, and without this nothing mounts
    // them. Ordered after the driver's start, which is only its fork, so
    // the script itself waits for the device. A oneshot: init goes on to
    // the next service once what fstab asked for is mounted.
    fs::write(
        svc_dir.join("ublk-mounts.service"),
        b"# What /etc/fstab puts on the disk the userspace driver serves\n\
          # (boot with udriver=virtio-blk@N). See\n\
          # /usr/local/bin/eclipse-ublk-mounts.\n\
          exec = /usr/local/bin/eclipse-ublk-mounts\n\
          type = oneshot\n\
          after = ublk\n\
          udriver = virtio-blk\n",
    )
    .unwrap();
    fs::write(localbin.join("eclipse-ublk-mounts"), UBLK_MOUNTS).unwrap();
}

/// `/usr/local/bin/eclipse-ublk-mounts`: every `/dev` line of fstab that
/// nothing has mounted, once its device is there.
///
/// Which lines are the served disk's is not asked: a line the kernel's own
/// pass mounted is skipped as mounted, and one it could not is this
/// script's whoever serves the device. The wait is one budget for all the
/// lines (`left`, in tenths of a second), so a fstab that names a disk
/// which never comes costs the boot 5 s once, not 5 s a line.
///
/// A mount point is looked up in `/proc/mounts` first because mounting
/// twice on one directory succeeds here, and stacks. (`mounted` reads into
/// names of its own: a shell function shares the caller's variables, and
/// reading the table into `at` left the mount point empty.)
const UBLK_MOUNTS: &[u8] = b"#!/bin/sh\n\
# Eclipse OS: mount what /etc/fstab puts on the disk a userspace driver\n\
# serves (udriver=virtio-blk@N). The kernel goes through fstab once, as\n\
# init starts, and that disk is not there yet: its driver is a service.\n\
# This runs after the driver's start, waits for each device fstab names\n\
# that nothing has mounted, and mounts it. A device that does not appear\n\
# within the wait (5 s for all of them together) is reported and left.\n\
fstab=/etc/fstab\n\
[ -r \"$fstab\" ] || exit 0\n\
left=50\n\
rc=0\n\
mounted() {\n\
    while read -r _ place _; do\n\
        [ \"$place\" = \"$1\" ] && return 0\n\
    done < /proc/mounts\n\
    return 1\n\
}\n\
while read -r dev at type opts _; do\n\
    case \"$dev\" in /dev/*) ;; *) continue ;; esac\n\
    case \"$at\" in /?*) ;; *) continue ;; esac\n\
    case \",$opts,\" in *,noauto,*) continue ;; esac\n\
    mounted \"$at\" && continue\n\
    while [ ! -e \"$dev\" ] && [ \"$left\" -gt 0 ]; do\n\
        sleep 0.1\n\
        left=$((left - 1))\n\
    done\n\
    if [ ! -e \"$dev\" ]; then\n\
        echo \"eclipse-ublk-mounts: $dev (for $at) is not there\" >&2\n\
        rc=1\n\
        continue\n\
    fi\n\
    mkdir -p \"$at\"\n\
    busybox mount -t \"$type\" -o \"$opts\" \"$dev\" \"$at\" || rc=1\n\
done < \"$fstab\"\n\
exit $rc\n";

impl LinuxRootfs {
    /// Put the drivers and their tools in the image's `/bin`, built for its
    /// architecture.
    pub(super) fn install_udrivers(&self, musl: &Path, bin: &Path) {
        let put = |built: &Path, name: &str| {
            if built.is_file() {
                let dst = bin.join(name);
                let _ = fs::remove_file(&dst);
                let _ = fs::copy(built, &dst);
            }
        };
        for name in TOOLS {
            put(&self.udriver_tool(musl, name), name);
        }
        let built = self.rust_tools(DRIVERS_PACKAGE, &DRIVERS, &[]);
        for name in DRIVERS {
            put(&built.join(name), name);
        }
        let built = self.rust_tools(NETSTACK_PACKAGE, &NETSTACK, &NETSTACK_USES);
        for name in NETSTACK {
            put(&built.join(name), name);
        }
    }

    /// Build the binaries `bins` of `tools/<package>` (Rust) as static,
    /// non-PIE musl binaries, like `eclipse-init`, and return the directory
    /// they are in. `uses` are the directories, from the top of the tree,
    /// of what the package takes by path. Best-effort: a binary that did
    /// not build is not there, and whatever wanted it says so on the boot
    /// that did.
    fn rust_tools(&self, package: &str, bins: &[&str], uses: &[&str]) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join(package);
        let triple = self.musl_rust_triple();
        let out = dir.join("target").join(triple).join("release");
        // Rebuild when a source file or the manifest is newer than any of
        // the binaries, or one is missing. The directory is the target's
        // own, so what is in it is never another architecture's.
        let newest = uses
            .iter()
            .map(|used| newest_mtime_under(&PROJECT_DIR.join(used)))
            .chain([
                newest_mtime_under(&dir.join("src")),
                fs::metadata(dir.join("Cargo.toml"))
                    .and_then(|m| m.modified())
                    .ok(),
            ])
            .flatten()
            .max();
        let fresh = |name: &str| {
            let built = fs::metadata(out.join(name)).and_then(|m| m.modified()).ok();
            matches!((built, newest), (Some(b), Some(s)) if b >= s)
        };
        if bins.iter().copied().all(fresh) {
            return out;
        }

        println!("Compiling {package} (Rust, {triple})...");
        let _ = optional_status(
            Ext::new("rustup").arg("target").arg("add").arg(triple),
            "install rustup so the musl target can be added",
        );
        // A build that fails must not leave the last one to be copied in
        // its place.
        for name in bins {
            let _ = fs::remove_file(out.join(name));
        }
        let mut cargo = Ext::new("cargo");
        cargo
            .current_dir(&dir)
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg(triple);
        // Static non-PIE, plus the cross linker when the host's `cc` cannot
        // link this target: see `cross_build_env`.
        for (k, v) in self.cross_build_env() {
            cargo.env(k, v);
        }
        if !cargo.status().success() {
            println!("Failed to compile {package}");
        }
        out
    }

    /// Compile `tools/<name>/<name>.c`, one of [`TOOLS`].
    fn udriver_tool(&self, musl: &Path, name: &str) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join(name);
        let executable = dir.join(name);
        let source = dir.join(format!("{name}.c"));
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

        println!("Compiling {name}...");
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
            println!("Failed to compile {name}");
            return executable;
        }

        Ext::new(strip).arg("-s").arg(&executable).status();
        executable
    }
}
