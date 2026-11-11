//! The drivers that run as processes, and the boot services that run them.
//!
//! A device the kernel was told to leave alone (`udriver=<kind>@<N>` on its
//! command line) is driven by a program in the image: `tools/zx-virtio-net`
//! for a virtio network card, `tools/zx-virtio-blk` for a virtio disk. Each
//! is a boot service -- `unet`, `ublk` -- that `eclipse-init` starts only on
//! a boot that left it a device (the `udriver =` key), watches, and starts
//! again when it dies. The kernel keeps what the driver gave it meanwhile:
//! the interface with its addresses and connections, the disk with what is
//! mounted on it.

use std::fs;
use std::path::{Path, PathBuf};

use super::LinuxRootfs;
use crate::PROJECT_DIR;
use os_xtask_utils::{CommandExt, Ext};

/// The drivers: `(directory under tools/, which is also the binary's name)`.
const DRIVERS: [&str; 2] = ["zx-virtio-net", "zx-virtio-blk"];

/// The `unet` and `ublk` services and the wrappers they run.
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
          # tools/zx-virtio-blk.\n\
          [ -x /bin/zx-virtio-blk ] || { echo 'eclipse-ublk: /bin/zx-virtio-blk is not in this image' >&2; sleep 5; exit 127; }\n\
          # Which disk: the first virtio-blk@N in the kernel's udriver= list.\n\
          n=$(tr ': ' '\\n\\n' < /proc/cmdline | sed -n 's/^udriver=//p' | tr ',' '\\n' | sed -n 's/^virtio-blk@//p' | head -n 1)\n\
          [ -n \"$n\" ] || { echo 'eclipse-ublk: no virtio-blk@N in udriver= on the kernel command line' >&2; sleep 5; exit 1; }\n\
          exec /bin/zx-virtio-blk service \"$n\"\n",
    )
    .unwrap();
}

impl LinuxRootfs {
    /// Put the drivers in the image's `/bin`, built for its architecture.
    pub(super) fn install_udrivers(&self, musl: &Path, bin: &Path) {
        for name in DRIVERS {
            let built = self.udriver(musl, name);
            if built.is_file() {
                let dst = bin.join(name);
                let _ = fs::remove_file(&dst);
                let _ = fs::copy(&built, &dst);
            }
        }
    }

    /// Compile `tools/<name>/<name>.c`, one of [`DRIVERS`].
    fn udriver(&self, musl: &Path, name: &str) -> PathBuf {
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
