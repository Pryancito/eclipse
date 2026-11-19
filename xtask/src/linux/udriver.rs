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
const DRIVERS: [&str; 4] = [
    "udrv-virtio-blk",
    "udrv-virtio-net",
    "udrv-nvme",
    "udrv-ahci",
];
/// What tests a node in `/dev` that a process serves, from the same
/// package: `udrv-dev-echo check NAME`.
const DEV_TESTS: [&str; 1] = ["udrv-dev-echo"];
/// The drivers of input devices, from the same package: each serves its
/// device as a node of `/dev/input` (`udriver=virtio-input@N`, and the
/// PC's i8042 with `udriver=i8042@0`: its keyboard and its mouse); and
/// what checks and times such a node, `udrv-evdev-check NODE`.
const INPUT_DRIVERS: [&str; 3] = ["udrv-virtio-input", "udrv-ps2", "udrv-evdev-check"];
/// The driver of an Intel HD Audio controller, from the same package,
/// serving it as `/dev/dsp` (`udriver=hda@N`); and what plays a tone
/// through such a node and checks it, `udrv-dsp-check`.
const AUDIO_DRIVERS: [&str; 2] = ["udrv-hda", "udrv-dsp-check"];
/// How many virtio input devices get a driver service of their own
/// (`uinput`, `uinput2`): a keyboard and a mouse or tablet.
const UINPUT_DEVICES: usize = 2;

/// The network stack that runs as a process: its package, under `tools/`,
/// its binaries -- the stack, and the UDP and TCP echo servers and the TCP
/// client it is measured with -- and what it
/// is built from besides its own sources (the drivers' library and the
/// tree's smoltcp, both by path).
const NETSTACK_PACKAGE: &str = "eclipse-netstack";
const NETSTACK: [&str; 4] = [
    "eclipse-netstack",
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
const FATFS: [&str; 3] = ["eclipse-fatfs", "eclipse-fs-bench", "eclipse-mountd"];
const FATFS_USES: [&str; 1] = ["tools/eclipse-udrivers/src"];
/// How many virtio disks get a driver service of their own (`ublk`,
/// `ublk2`, ...): the ones the kernel leaves beyond these have none.
const UBLK_DISKS: usize = 4;
/// The same for NVMe and AHCI controllers (`unvme`, `unvme2`, `uahci`,
/// `uahci2`).
const CONTROLLERS_SERVED: usize = 2;

/// The btrfs and EclipseFS filesystems that run as processes:
/// `eclipse-btrfs DEVICE DIR` and `eclipse-eclipsefs DEVICE DIR` mount a
/// volume they serve. They are built from the FAT server's library (the
/// wire and the kept disk) and the tree's btrfs and EclipseFS code.
const BTRFS_PACKAGE: &str = "eclipse-btrfs";
const BTRFS: [&str; 2] = ["eclipse-btrfs", "eclipse-eclipsefs"];
const BTRFS_USES: [&str; 5] = [
    "tools/eclipse-udrivers/src",
    "tools/eclipse-fatfs/src",
    "vendor/btrfs-rs/src",
    "vendor/eclipsefs-rs/src",
    "vendor/aes/src",
];

/// The NFS client that runs as a process: `eclipse-nfs HOST:/EXPORT DIR`
/// mounts an NFSv3 export it serves. Built from the FAT server's library
/// (the wire) and the kernel's NFS code (`vendor/nfs3-rs`).
const NFS_PACKAGE: &str = "eclipse-nfs";
const NFS: [&str; 1] = ["eclipse-nfs"];
const NFS_USES: [&str; 4] = [
    "tools/eclipse-udrivers/src",
    "tools/eclipse-fatfs/src",
    "vendor/nfs3-rs/src",
    "vendor/rcore-fs/src",
];

/// The `unet` and `ublk` services and the wrappers they run, and
/// `fs-mounts`, which mounts fstab's FAT, btrfs, EclipseFS and NFS volumes
/// with a server each, on the served disks too.
///
/// `udriver =` keeps each out of every boot where the kernel drives the
/// device itself or the machine has none: the virtio disks and the first
/// virtio network card are left unless the command line says otherwise
/// (`ublk=kernel` keeps the disks in the kernel). Supervised: when a driver dies the kernel resets its device
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

    // The disks: every virtio disk the kernel left, named on the command
    // line or not (it leaves them all unasked, but the one it boots from
    // on a machine with no initrd). Which they are depends on the machine,
    // so each service is gated on the Nth disk of the kernel's list and
    // the wrapper reads its number there: `ublk` drives the first, `ublk2`
    // the second, up to four.
    for nth in 1..=UBLK_DISKS {
        let (name, which, arg) = match nth {
            1 => ("ublk".to_string(), "virtio-blk".to_string(), String::new()),
            n => (
                format!("ublk{n}"),
                format!("virtio-blk/{n}"),
                format!(" {n}"),
            ),
        };
        fs::write(
            svc_dir.join(format!("{name}.service")),
            format!(
                "# Driver of virtio disk number {nth} of those the kernel left to\n\
                 # userspace (all of them, unless the boot says ublk=kernel or\n\
                 # names one with udriver=virtio-blk@N). See\n\
                 # /usr/local/bin/eclipse-ublk.\n\
                 exec = /usr/local/bin/eclipse-ublk{arg}\n\
                 type = respawn\n\
                 udriver = {which}\n"
            ),
        )
        .unwrap();
    }
    fs::write(
        localbin.join("eclipse-ublk"),
        b"#!/bin/sh\n\
          # Eclipse OS: the userspace driver of a virtio disk the kernel\n\
          # left alone, in the foreground for eclipse-init, which starts it\n\
          # again if it dies. The kernel keeps the disk meanwhile, with what\n\
          # is mounted on it, and what was being read or written goes on with\n\
          # the next driver. See tools/eclipse-udrivers.\n\
          # Which disk: the Nth virtio-blk@K the kernel lists in /proc/udrivers\n\
          # (N is the argument, 1 without one); on a kernel with no such list,\n\
          # the Nth of the udriver= words on its command line.\n\
          nth=${1:-1}\n\
          if [ -r /proc/udrivers ]; then\n\
          \x20   disks=$(sed -n 's/^virtio-blk@//p' /proc/udrivers)\n\
          else\n\
          \x20   disks=$(tr ': ' '\\n\\n' < /proc/cmdline | sed -n 's/^udriver=//p' | tr ',' '\\n' | sed -n 's/^virtio-blk@//p')\n\
          fi\n\
          n=$(echo \"$disks\" | sed -n \"${nth}p\")\n\
          [ -n \"$n\" ] || { echo \"eclipse-ublk: the kernel left no virtio disk number $nth to userspace\" >&2; sleep 5; exit 1; }\n\
          # The driver is the Rust one and only that one: the C tool's own\n\
          # driver is for checking and timing, never the system's.\n\
          [ -x /bin/udrv-virtio-blk ] || { echo 'eclipse-ublk: /bin/udrv-virtio-blk is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }\n\
          exec /bin/udrv-virtio-blk \"$n\"\n",
    )
    .unwrap();

    // The NVMe and AHCI controllers the boot leaves to userspace
    // (`udriver=nvme@N`, `udriver=ahci@N`): one service for each of the
    // first two of a kind, the Nth of the kernel's list, and a wrapper
    // that finds its number there.
    for (service, kind, what, driver) in [
        ("unvme", "nvme", "NVMe controller", "udrv-nvme"),
        (
            "uahci",
            "ahci",
            "AHCI controller (every SATA disk on it)",
            "udrv-ahci",
        ),
    ] {
        for nth in 1..=CONTROLLERS_SERVED {
            let (name, which) = match nth {
                1 => (service.to_string(), kind.to_string()),
                n => (format!("{service}{n}"), format!("{kind}/{n}")),
            };
            fs::write(
                svc_dir.join(format!("{name}.service")),
                format!(
                    "# Driver of {what} number {nth} of those the kernel\n\
                     # left to userspace (udriver={kind}@N). See\n\
                     # /usr/local/bin/eclipse-{service}.\n\
                     exec = /usr/local/bin/eclipse-{service} {nth}\n\
                     type = respawn\n\
                     udriver = {which}\n"
                ),
            )
            .unwrap();
        }
        fs::write(
            localbin.join(format!("eclipse-{service}")),
            format!(
                "#!/bin/sh\n\
                 # Eclipse OS: the userspace driver of an {what} the\n\
                 # kernel left alone (udriver={kind}@N), in the foreground for\n\
                 # eclipse-init, which starts it again if it dies. The kernel keeps\n\
                 # the disks meanwhile, with what is mounted on them. See\n\
                 # tools/eclipse-udrivers.\n\
                 # Which controller: the Nth {kind}@K of /proc/udrivers (N is the\n\
                 # argument, 1 without one).\n\
                 nth=${{1:-1}}\n\
                 n=$(sed -n 's/^{kind}@//p' /proc/udrivers | sed -n \"${{nth}}p\")\n\
                 [ -n \"$n\" ] || {{ echo \"eclipse-{service}: the kernel left no {kind} number $nth to userspace\" >&2; sleep 5; exit 1; }}\n\
                 [ -x /bin/{driver} ] || {{ echo 'eclipse-{service}: /bin/{driver} is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }}\n\
                 exec /bin/{driver} \"$n\"\n"
            ),
        )
        .unwrap();
    }

    // The virtio input devices the boot leaves to userspace
    // (`udriver=virtio-input@N`): one service for each of the first two,
    // the Nth of the kernel's list, each serving its device as a node of
    // `/dev/input`.
    for nth in 1..=UINPUT_DEVICES {
        let (name, which) = match nth {
            1 => ("uinput".to_string(), "virtio-input".to_string()),
            n => (format!("uinput{n}"), format!("virtio-input/{n}")),
        };
        fs::write(
            svc_dir.join(format!("{name}.service")),
            format!(
                "# Driver of virtio input device number {nth} of those the kernel\n\
                 # left to userspace (udriver=virtio-input@N), served as\n\
                 # /dev/input/eventK. See /usr/local/bin/eclipse-uinput.\n\
                 exec = /usr/local/bin/eclipse-uinput {nth}\n\
                 type = respawn\n\
                 udriver = {which}\n"
            ),
        )
        .unwrap();
    }
    fs::write(
        localbin.join("eclipse-uinput"),
        b"#!/bin/sh\n\
          # Eclipse OS: the userspace driver of a virtio input device the\n\
          # kernel left alone (udriver=virtio-input@N), in the foreground for\n\
          # eclipse-init, which starts it again if it dies. It serves the\n\
          # device as /dev/input/eventK. See tools/eclipse-udrivers.\n\
          # Which device: the Nth virtio-input@K of /proc/udrivers (N is the\n\
          # argument, 1 without one).\n\
          nth=${1:-1}\n\
          n=$(sed -n 's/^virtio-input@//p' /proc/udrivers | sed -n \"${nth}p\")\n\
          [ -n \"$n\" ] || { echo \"eclipse-uinput: the kernel left no virtio input device number $nth to userspace\" >&2; sleep 5; exit 1; }\n\
          [ -x /bin/udrv-virtio-input ] || { echo 'eclipse-uinput: /bin/udrv-virtio-input is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }\n\
          exec /bin/udrv-virtio-input \"$n\"\n",
    )
    .unwrap();

    // The PC's i8042 (`udriver=i8042@0`, x86_64 only): one driver for its
    // keyboard and its mouse, two nodes of `/dev/input`.
    fs::write(
        svc_dir.join("ups2.service"),
        "# Driver of the i8042 (PS/2 keyboard and mouse) when the kernel\n\
         # left it to userspace (udriver=i8042@0), served as two\n\
         # /dev/input/eventK. See /usr/local/bin/eclipse-ups2.\n\
         exec = /usr/local/bin/eclipse-ups2\n\
         type = respawn\n\
         udriver = i8042\n",
    )
    .unwrap();
    fs::write(
        localbin.join("eclipse-ups2"),
        b"#!/bin/sh\n\
          # Eclipse OS: the userspace driver of the i8042 the kernel left\n\
          # alone (udriver=i8042@0), in the foreground for eclipse-init, which\n\
          # starts it again if it dies. It serves the keyboard and the mouse\n\
          # as /dev/input/eventK. See tools/eclipse-udrivers.\n\
          grep -qx 'i8042@0' /proc/udrivers || { echo 'eclipse-ups2: the kernel left no i8042 to userspace' >&2; sleep 5; exit 1; }\n\
          [ -x /bin/udrv-ps2 ] || { echo 'eclipse-ups2: /bin/udrv-ps2 is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }\n\
          exec /bin/udrv-ps2\n",
    )
    .unwrap();

    // The HD Audio controller the boot leaves to userspace (`udriver=hda@N`):
    // the first one, served as `/dev/dsp`.
    fs::write(
        svc_dir.join("uhda.service"),
        "# Driver of the first Intel HD Audio controller the kernel left to\n\
         # userspace (udriver=hda@N), served as /dev/dsp. See\n\
         # /usr/local/bin/eclipse-uhda.\n\
         exec = /usr/local/bin/eclipse-uhda\n\
         type = respawn\n\
         udriver = hda\n",
    )
    .unwrap();
    fs::write(
        localbin.join("eclipse-uhda"),
        b"#!/bin/sh\n\
          # Eclipse OS: the userspace driver of an HD Audio controller the\n\
          # kernel left alone (udriver=hda@N), in the foreground for\n\
          # eclipse-init, which starts it again if it dies. It serves the\n\
          # controller as /dev/dsp. See tools/eclipse-udrivers.\n\
          n=$(sed -n 's/^hda@//p' /proc/udrivers | sed -n 1p)\n\
          [ -n \"$n\" ] || { echo 'eclipse-uhda: the kernel left no HD Audio controller to userspace' >&2; sleep 5; exit 1; }\n\
          [ -x /bin/udrv-hda ] || { echo 'eclipse-uhda: /bin/udrv-hda is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }\n\
          exec /bin/udrv-hda \"$n\"\n",
    )
    .unwrap();

    // Gone: what fstab puts on a disk a process drives is fs-mounts's now
    // (every type it has is one a process serves), which waits for that
    // disk as this did.
    let _ = fs::remove_file(svc_dir.join("ublk-mounts.service"));
    let _ = fs::remove_file(localbin.join("eclipse-ublk-mounts"));

    // The FAT, btrfs, EclipseFS and NFS lines of `/etc/fstab`: each is
    // mounted by a process that serves it, not by the kernel, which leaves
    // those lines when the root filesystem has this service. On every
    // boot, whoever drives the disk; after the userspace disk driver's
    // start for the boots that have one, and the mount helper's.
    let _ = fs::remove_file(svc_dir.join("fat-mounts.service"));
    let _ = fs::remove_file(localbin.join("eclipse-fat-mounts"));
    fs::write(
        svc_dir.join("fs-mounts.service"),
        b"# The FAT, btrfs, EclipseFS and NFS lines of /etc/fstab, each\n\
          # served by a process (ufs=kernel keeps all but FAT for the\n\
          # kernel). See /usr/local/bin/eclipse-fs-mounts.\n\
          exec = /usr/local/bin/eclipse-fs-mounts\n\
          type = oneshot\n\
          after = ublk ublk2 ublk3 ublk4 unvme unvme2 uahci uahci2 mountd\n",
    )
    .unwrap();
    fs::write(localbin.join("eclipse-fs-mounts"), FS_MOUNTS).unwrap();

    // The mount helper: the kernel hands it each mount(2) of a filesystem
    // a process serves, which it makes with that server. On every boot, as
    // mount(2) is.
    fs::write(
        svc_dir.join("mountd.service"),
        b"# The mount helper: the kernel asks this process to make each\n\
          # mount(2) of FAT, btrfs, EclipseFS and NFS, which it does with\n\
          # their servers. While it is not running, mount(2) of FAT is\n\
          # ENODEV and the others are the kernel's. See\n\
          # /usr/local/bin/eclipse-mountd.\n\
          exec = /usr/local/bin/eclipse-mountd\n\
          type = respawn\n",
    )
    .unwrap();
    fs::write(
        localbin.join("eclipse-mountd"),
        b"#!/bin/sh\n\
          # Eclipse OS: the mount helper (tools/eclipse-fatfs), in the\n\
          # foreground for eclipse-init, which starts it again if it dies.\n\
          [ -x /bin/eclipse-mountd ] || { echo 'eclipse-mountd: /bin/eclipse-mountd is not in this image (its build failed: see the image log)' >&2; sleep 5; exit 127; }\n\
          exec /bin/eclipse-mountd\n",
    )
    .unwrap();
}

/// `/usr/local/bin/eclipse-fs-mounts`: every FAT, btrfs, EclipseFS and
/// NFS line of fstab that the kernel's pass left, mounted with a server
/// process.
///
/// The kernel's pass leaves them (`left_to_server`, in the kernel): every
/// FAT line, as the kernel has no FAT, and the others unless the boot says
/// `ufs=kernel` or the line asks what a served mount drops (`nosuid`,
/// `noexec`, `nodev`, a btrfs or EclipseFS option its server does not
/// read). The types are the kernel's spellings.
///
/// A FAT line is mounted with `eclipse-fatfs`. The others go through
/// `mount(2)`, which applies that rule again: it hands them to the mount
/// helper (`eclipse-mountd`) and its servers, and to the kernel's own code
/// for a volume they do not take or a line the kernel keeps. So this takes
/// every such line nothing has mounted -- a line the kernel kept and could
/// not mount yet, its disk served by a process, as well -- and waits for
/// the helper first, three seconds at most, unless the boot keeps these
/// for the kernel. NFS lines come last, each tried three
/// times five seconds apart: the network may still be coming up.
///
/// The placeholder of an image nobody installed (`__ECLIPSE_EFI_DEV___`)
/// at `/boot/efi` is the first partition of the disk `ROOT=` names, as
/// the kernel's pass used to make it. A volume that is not mounted is said
/// and left. The wait for a device is for the disk a userspace driver
/// serves, which comes after init starts; every other disk is there by now
/// or is not coming.
///
/// The mounting is left running and the script ends: init goes on to the
/// next service at once. On one processor a server's start among all the
/// others' took the boot 0.3 s that the kernel's pass, a task beside init,
/// never did; nothing is ordered after these mounts, as nothing could be
/// after the kernel's.
const FS_MOUNTS: &[u8] = b"#!/bin/sh\n\
# Eclipse OS: mount the FAT, btrfs, EclipseFS and NFS lines of\n\
# /etc/fstab, each with a process that serves it. The kernel leaves those\n\
# lines to this service; ufs=kernel on its command line keeps all but FAT\n\
# for the kernel.\n\
fstab=/etc/fstab\n\
[ -r \"$fstab\" ] || exit 0\n\
# (No program is run to read the boot's words or a line's type: on the\n\
# boot with no such line this costs one shell and nothing else.)\n\
# The first partition of ROOT='s disk, for an EFI placeholder.\n\
efi=\n\
kernel=\n\
read -r words < /proc/cmdline\n\
set -f\n\
IFS=': \t'\n\
for word in $words; do\n\
    case \"$word\" in\n\
        ROOT=/dev/*[0-9]) efi=${word#ROOT=} ;;\n\
        ufs=kernel) kernel=1 ;;\n\
        ufs=*) kernel= ;;\n\
    esac\n\
done\n\
unset IFS\n\
set +f\n\
case \"$efi\" in /dev/__ECLIPSE_*) efi= ;; esac\n\
# /dev/sda2 is /dev/sda1, /dev/nvme0n1p2 /dev/nvme0n1p1.\n\
while [ -n \"$efi\" ]; do\n\
    case \"$efi\" in *[0-9]) efi=${efi%?} ;; *) efi=${efi}1; break ;; esac\n\
done\n\
# Only the disk of a userspace driver is waited for (5 s for all the\n\
# lines together): the kernel's disks are there before init.\n\
left=0\n\
while read -r device; do\n\
    case \"$device\" in virtio-blk@*|nvme@*|ahci@*) left=50 ;; esac\n\
done < /proc/udrivers\n\
# The mount helper, once (3 s): it says it is there in /run.\n\
helper=30\n\
mounted() {\n\
    while read -r _ place _; do\n\
        [ \"$place\" = \"$1\" ] && return 0\n\
    done < /proc/mounts\n\
    return 1\n\
}\n\
# The kind of a line's type, in the kernel's spellings, or nothing.\n\
kind_of() {\n\
    case \"$1\" in\n\
        [Vv][Ff][Aa][Tt]|[Ff][Aa][Tt]|[Ff][Aa][Tt]32|[Ff][Aa][Tt]16|[Mm][Ss][Dd][Oo][Ss]) kind=fat ;;\n\
        [Bb][Tt][Rr][Ff][Ss]) kind=block ;;\n\
        [Ee][Cc][Ll][Ii][Pp][Ss][Ee][Ff][Ss]) kind=block ;;\n\
        [Nn][Ff][Ss]|[Nn][Ff][Ss]3) kind=nfs ;;\n\
        *) kind= ;;\n\
    esac\n\
}\n\
wait_helper() {\n\
    [ -z \"$kernel\" ] || return 0\n\
    while [ ! -e /run/eclipse-mountd ] && [ \"$helper\" -gt 0 ]; do\n\
        sleep 0.1\n\
        helper=$((helper - 1))\n\
    done\n\
}\n\
mounts() {\n\
    while read -r dev at type opts _; do\n\
        case \"$at\" in /?*) ;; *) continue ;; esac\n\
        case \",$opts,\" in *,noauto,*) continue ;; esac\n\
        kind_of \"$type\"\n\
        case \"$kind\" in fat|block) ;; *) continue ;; esac\n\
        case \"$dev\" in\n\
            /dev/*) ;;\n\
            __ECLIPSE_EFI*|/__ECLIPSE_EFI*)\n\
                [ \"$kind\" = fat ] && [ \"$at\" = /boot/efi ] && [ -n \"$efi\" ] || continue\n\
                dev=$efi ;;\n\
            *) continue ;;\n\
        esac\n\
        mounted \"$at\" && continue\n\
        while [ ! -e \"$dev\" ] && [ \"$left\" -gt 0 ]; do\n\
            sleep 0.1\n\
            left=$((left - 1))\n\
        done\n\
        if [ ! -e \"$dev\" ]; then\n\
            echo \"eclipse-fs-mounts: $dev (for $at) is not there\" >&2\n\
            continue\n\
        fi\n\
        [ -d \"$at\" ] || mkdir -p \"$at\"\n\
        if [ \"$kind\" = fat ]; then\n\
            ro=\n\
            case \",$opts,\" in *,ro,*) ro=-r ;; esac\n\
            /bin/eclipse-fatfs $ro \"$dev\" \"$at\" ||\n\
                echo \"eclipse-fs-mounts: no server mounted $dev at $at\" >&2\n\
        else\n\
            wait_helper\n\
            busybox mount -t \"$type\" -o \"${opts:-defaults}\" \"$dev\" \"$at\" ||\n\
                echo \"eclipse-fs-mounts: $dev was not mounted at $at\" >&2\n\
        fi\n\
    done < \"$fstab\"\n\
    while read -r dev at type opts _; do\n\
        case \"$at\" in /?*) ;; *) continue ;; esac\n\
        case \",$opts,\" in *,noauto,*) continue ;; esac\n\
        kind_of \"$type\"\n\
        [ \"$kind\" = nfs ] || continue\n\
        case \"$dev\" in ?*:/*) ;; *) continue ;; esac\n\
        mounted \"$at\" && continue\n\
        [ -d \"$at\" ] || mkdir -p \"$at\"\n\
        wait_helper\n\
        for try in 1 2 3; do\n\
            busybox mount -t \"$type\" -o \"${opts:-defaults}\" \"$dev\" \"$at\" && break\n\
            [ \"$try\" = 3 ] && echo \"eclipse-fs-mounts: $dev was not mounted at $at\" >&2\n\
            [ \"$try\" = 3 ] || sleep 5\n\
        done\n\
    done < \"$fstab\"\n\
}\n\
# Not waited for: the kernel's pass over fstab, which these lines were\n\
# mounted by, runs beside init too and never held the boot back.\n\
mounts &\n\
exit 0\n";

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
        let built = self.rust_tools(DRIVERS_PACKAGE, &DEV_TESTS, &[]);
        for name in DEV_TESTS {
            put(&built.join(name), name);
        }
        let built = self.rust_tools(DRIVERS_PACKAGE, &INPUT_DRIVERS, &[]);
        for name in INPUT_DRIVERS {
            put(&built.join(name), name);
        }
        let built = self.rust_tools(DRIVERS_PACKAGE, &AUDIO_DRIVERS, &[]);
        for name in AUDIO_DRIVERS {
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
        let built = self.rust_tools(BTRFS_PACKAGE, &BTRFS, &BTRFS_USES);
        for name in BTRFS {
            put(&built.join(name), name);
        }
        let built = self.rust_tools(NFS_PACKAGE, &NFS, &NFS_USES);
        for name in NFS {
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
