//! The drivers that run as processes, and the boot services that run them.
//!
//! A device the kernel leaves alone -- one its command line names
//! (`udriver=<kind>@<N>`), and the first virtio network card without a
//! word, the kernel having no driver for it -- is driven by a program in
//! the image. Each is a boot service -- `unet`, `ublk` -- that
//! `eclipse-init` starts only on a boot that left it a device (the
//! `udriver =` key, against the kernel's `/proc/udrivers`), watches, and
//! starts again when it dies. The kernel keeps what the driver gave it meanwhile: the
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
//! process, and what the `unet` service runs on a machine with a virtio
//! network card: it is the card's driver itself, takes
//! the card's frames instead of the kernel and asks a DHCP server for its
//! address, or has the one the boot gives it (`unetstack=ADDR/PREFIX`).
//! With `unetdriver=process` as well it runs `udrv-virtio-net` for the
//! card, a process of its own. `unetstack=none` is the boot where
//! nothing of the system's takes the card: the kernel does not leave it
//! unasked, and if the boot names it the service only waits, for a driver
//! run by hand. Whose sockets that stack's are is the kernel's to say
//! (`linux_object::net::user_stack::boot`).
//!
//! On a machine with no virtio card the stack's card is the first one the
//! kernel drives itself, lent: the kernel lists it as `kernel-net@0`, the
//! `knet` service is the one that starts, and the same wrapper runs the
//! stack with `card=kernel`, which borrows the card instead of claiming a
//! device.
//!
//! An Intel 82574 card (QEMU's `e1000e`) the boot leaves to userspace
//! (an x86_64 machine with no virtio card does, unless it says
//! `unetcard=kernel`; `udriver=e1000e@0` elsewhere) is the stack's too, and the stack is its driver:
//! the `enet` service starts, and the wrapper runs the stack with
//! `card=e1000e`.

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

/// A filesystem that runs as a process: its package, under `tools/`, its
/// binaries -- `eclipse-fatfs DEVICE DIR` mounts a FAT volume it serves,
/// and `eclipse-fs-bench` is what a mount is timed with -- and what it is
/// built from besides its own sources.
const FATFS_PACKAGE: &str = "eclipse-fatfs";
const FATFS: [&str; 2] = ["eclipse-fatfs", "eclipse-fs-bench"];
const FATFS_USES: [&str; 1] = ["tools/eclipse-udrivers/src"];

/// The `unet` and `ublk` services and the wrappers they run, and
/// `ublk-mounts`, which mounts what fstab puts on the served disk; and
/// `fat-mounts`, which mounts fstab's FAT volumes with a server each.
///
/// `udriver =` keeps each out of every boot where the kernel drives the
/// device itself or the machine has none: a disk is left when the command
/// line says so, and the first virtio network card unless it says
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
        b"# The virtio network card, which the kernel leaves to userspace\n\
          # (it has no driver for it): the network stack that runs as a\n\
          # process, or the card's driver alone. See\n\
          # /usr/local/bin/eclipse-unet.\n\
          exec = /usr/local/bin/eclipse-unet\n\
          type = respawn\n\
          udriver = virtio-net@0\n",
    )
    .unwrap();
    // The kernel's own first card, on a boot that lends it (one with no
    // virtio card): the kernel lists it beside what it left.
    fs::write(
        svc_dir.join("knet.service"),
        b"# The first network card the kernel drives, lent to the network\n\
          # stack that runs as a process (a machine with no virtio card). See\n\
          # /usr/local/bin/eclipse-unet.\n\
          exec = /usr/local/bin/eclipse-unet kernel\n\
          type = respawn\n\
          udriver = kernel-net@0\n",
    )
    .unwrap();
    // The first e1000e, when the boot leaves it to userspace: the stack
    // claims it and drives it itself.
    fs::write(
        svc_dir.join("enet.service"),
        b"# The first e1000e network card, left to userspace by the boot\n\
          # (udriver=e1000e@0): the network stack that runs as a process, which\n\
          # is its driver too. See /usr/local/bin/eclipse-unet.\n\
          exec = /usr/local/bin/eclipse-unet e1000e\n\
          type = respawn\n\
          udriver = e1000e@0\n",
    )
    .unwrap();
    fs::write(
        localbin.join("eclipse-unet"),
        b"#!/bin/sh\n\
          # Eclipse OS: what drives the virtio network card, which the\n\
          # kernel leaves alone, in the foreground for eclipse-init, which\n\
          # starts it again if it dies. See tools/eclipse-udrivers.\n\
          # The driver is the Rust one and only that one: the C tool's own\n\
          # driver is for checking and timing, never the system's.\n\
          #\n\
          # eclipse-unet kernel: the stack's card is the first one the kernel\n\
          # drives itself, which it lends when there is no virtio card.\n\
          # No driver runs here then: the kernel's is the card's.\n\
          # eclipse-unet e1000e: the stack's card is the first e1000e, which\n\
          # the boot left to userspace (udriver=e1000e@0); the stack drives it.\n\
          [ \"$1\" = kernel ] || [ \"$1\" = e1000e ] || [ -x /bin/udrv-virtio-net ] || { echo 'eclipse-unet: /bin/udrv-virtio-net is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }\n\
          # The card does not go to the kernel: the network stack that runs\n\
          # as a process takes it and is its driver too. See\n\
          # tools/eclipse-netstack. It asks a DHCP server for its address\n\
          # and gateway (unetstack=dhcp on the kernel command line, or no\n\
          # such word); unetstack=ADDR/PREFIX gives it that address, and\n\
          # unetstack=ADDR/PREFIX,GATEWAY a gateway too.\n\
          # unetstack=kernel kept the kernel's own stack once, and says\n\
          # nothing now: the stack has the card as with no such word.\n\
          # unetstack=none: nothing of the system's takes the card. The\n\
          # kernel leaves it only if udriver=virtio-net@0 says so, and then\n\
          # it is for a driver run by hand: this only waits.\n\
          words=$(tr ': ' '\\n\\n' < /proc/cmdline)\n\
          stack=$(echo \"$words\" | sed -n 's/^unetstack=//p' | head -n 1)\n\
          [ -n \"$stack\" ] && [ \"$stack\" != kernel ] || stack=dhcp\n\
          if [ \"$stack\" = none ]; then\n\
          \x20   while sleep 86400; do :; done\n\
          \x20   exit 0\n\
          fi\n\
          [ -x /bin/eclipse-netstack ] || { echo 'eclipse-unet: /bin/eclipse-netstack is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }\n\
          # unetdriver=process: the driver in a process of its own, which\n\
          # the stack starts and starts again, as it was before the stack\n\
          # drove the card itself. A driver that dies then costs no\n\
          # connection, and every frame a change of process each way.\n\
          case $1:$(echo \"$words\" | sed -n 's/^unetdriver=//p' | head -n 1) in\n\
          \x20   kernel:*) set -- card=kernel ;;\n\
          \x20   e1000e:*) set -- card=e1000e ;;\n\
          \x20   :process) set -- driver=process ;;\n\
          \x20   *) set -- ;;\n\
          esac\n\
          exec /bin/eclipse-netstack 0 \"$stack\" \"$@\"\n",
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

    // The FAT volumes of `/etc/fstab`: each is mounted by a process that
    // serves it (`eclipse-fatfs`), not by the kernel's FAT, which leaves
    // those lines when the root filesystem has this service. On every
    // boot, whoever drives the disk; after the userspace disk driver's
    // start for the boots that have one.
    fs::write(
        svc_dir.join("fat-mounts.service"),
        b"# The FAT volumes of /etc/fstab, each served by a process
          # (eclipse-fatfs); ufat=kernel on the kernel command line keeps
          # them for the kernel's own FAT. See
          # /usr/local/bin/eclipse-fat-mounts.
          exec = /usr/local/bin/eclipse-fat-mounts
          type = oneshot
          after = ublk
",
    )
    .unwrap();
    fs::write(localbin.join("eclipse-fat-mounts"), FAT_MOUNTS).unwrap();
}

/// `/usr/local/bin/eclipse-fat-mounts`: every FAT line of fstab on a
/// device, mounted with a server process.
///
/// The kernel's pass over fstab leaves these lines when the root
/// filesystem has `fat-mounts.service` and the boot does not say
/// `ufat=kernel` (`left_to_fat_server`, in the kernel); with the word the
/// kernel mounts them and this does nothing. The types are the kernel's
/// spellings of FAT, in any case, so no line is left by one and not taken
/// by the other.
///
/// A volume whose server does not make the mount (no binary, a volume it
/// cannot read) is mounted with the kernel's FAT, and said: `/boot/efi`
/// is there either way. The wait for a device is for the disk a userspace
/// driver serves, which comes after init starts; every other disk is
/// there by now or is not coming.
///
/// The mounting is left running and the script ends: init goes on to the
/// next service at once. On one processor a server's start among all the
/// others' took the boot 0.3 s that the kernel's pass, a task beside init,
/// never did; nothing is ordered after these mounts, as nothing could be
/// after the kernel's.
const FAT_MOUNTS: &[u8] = b"#!/bin/sh\n\
# Eclipse OS: mount the FAT volumes of /etc/fstab, each with a process\n\
# that serves it (eclipse-fatfs) and not with the kernel's FAT. The\n\
# kernel leaves those lines to this service; ufat=kernel on its command\n\
# line keeps them for the kernel, and then there is nothing to do here.\n\
# A volume the server does not mount is mounted with the kernel's FAT.\n\
fstab=/etc/fstab\n\
[ -r \"$fstab\" ] || exit 0\n\
# (No program is run to read the boot's words or a line's type: on the\n\
# boot with no FAT line this costs one shell and nothing else.)\n\
read -r words < /proc/cmdline\n\
set -f\n\
IFS=': \t'\n\
for word in $words; do\n\
    case \"$word\" in ufat=*) [ \"$word\" = ufat=kernel ] && exit 0; break ;; esac\n\
done\n\
unset IFS\n\
set +f\n\
# Only the disk of a userspace driver is waited for (5 s for all the\n\
# lines together): the kernel's disks are there before init.\n\
left=0\n\
while read -r device; do\n\
    case \"$device\" in virtio-blk@*) left=50 ;; esac\n\
done < /proc/udrivers\n\
mounted() {\n\
    while read -r _ place _; do\n\
        [ \"$place\" = \"$1\" ] && return 0\n\
    done < /proc/mounts\n\
    return 1\n\
}\n\
mounts() {\n\
    while read -r dev at type opts _; do\n\
        case \"$dev\" in /dev/*) ;; *) continue ;; esac\n\
        case \"$at\" in /?*) ;; *) continue ;; esac\n\
        case \"$type\" in\n\
            [Vv][Ff][Aa][Tt]|[Ff][Aa][Tt]|[Ff][Aa][Tt]32|[Ff][Aa][Tt]16|[Mm][Ss][Dd][Oo][Ss]) ;;\n\
            *) continue ;;\n\
        esac\n\
        case \",$opts,\" in *,noauto,*) continue ;; esac\n\
        mounted \"$at\" && continue\n\
        while [ ! -e \"$dev\" ] && [ \"$left\" -gt 0 ]; do\n\
            sleep 0.1\n\
            left=$((left - 1))\n\
        done\n\
        if [ ! -e \"$dev\" ]; then\n\
            echo \"eclipse-fat-mounts: $dev (for $at) is not there\" >&2\n\
            continue\n\
        fi\n\
        [ -d \"$at\" ] || mkdir -p \"$at\"\n\
        ro=\n\
        case \",$opts,\" in *,ro,*) ro=-r ;; esac\n\
        /bin/eclipse-fatfs $ro \"$dev\" \"$at\" && continue\n\
        echo \"eclipse-fat-mounts: no server for $dev at $at: the kernel's FAT mounts it\" >&2\n\
        busybox mount -t \"$type\" -o \"$opts\" \"$dev\" \"$at\"\n\
    done < \"$fstab\"\n\
}\n\
# Not waited for: the kernel's pass over fstab, which these lines were\n\
# mounted by, runs beside init too and never held the boot back.\n\
mounts &\n\
exit 0\n";

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
///
/// The FAT lines are not this script's but [`FAT_MOUNTS`]'s, except on
/// the boot that keeps FAT in the kernel (`ufat=kernel`).
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
# The FAT volumes are eclipse-fat-mounts's, which mounts each with a\n\
# server process, unless the boot keeps them for the kernel's FAT.\n\
fat=\n\
read -r words < /proc/cmdline\n\
set -f\n\
IFS=': \t'\n\
for word in $words; do\n\
    case \"$word\" in ufat=*) fat=${word#ufat=}; break ;; esac\n\
done\n\
unset IFS\n\
set +f\n\
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
    if [ \"$fat\" != kernel ]; then\n\
        case \"$type\" in\n\
            [Vv][Ff][Aa][Tt]|[Ff][Aa][Tt]|[Ff][Aa][Tt]32|[Ff][Aa][Tt]16|[Mm][Ss][Dd][Oo][Ss]) continue ;;\n\
        esac\n\
    fi\n\
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
        let built = self.rust_tools(FATFS_PACKAGE, &FATFS, &FATFS_USES);
        for name in FATFS {
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
