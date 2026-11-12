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
//! `tools/eclipse-udrivers`: `udrv-virtio-blk` for a virtio disk. The C
//! programs in `tools/zx-virtio-blk` and `tools/zx-virtio-net` are the
//! tools that check and time a driver; each also carries a driver of its
//! own (`service`), which is what the network card still runs, and what the
//! disk falls back to in an image whose Rust build failed.

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
const DRIVERS: [&str; 1] = ["udrv-virtio-blk"];

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
          # tools/eclipse-udrivers.\n\
          # Which disk: the first virtio-blk@N in the kernel's udriver= list.\n\
          n=$(tr ': ' '\\n\\n' < /proc/cmdline | sed -n 's/^udriver=//p' | tr ',' '\\n' | sed -n 's/^virtio-blk@//p' | head -n 1)\n\
          [ -n \"$n\" ] || { echo 'eclipse-ublk: no virtio-blk@N in udriver= on the kernel command line' >&2; sleep 5; exit 1; }\n\
          [ -x /bin/udrv-virtio-blk ] && exec /bin/udrv-virtio-blk \"$n\"\n\
          # An image whose Rust build failed: the C tool carries a driver too.\n\
          echo 'eclipse-ublk: /bin/udrv-virtio-blk is not in this image, trying zx-virtio-blk' >&2\n\
          [ -x /bin/zx-virtio-blk ] || { echo 'eclipse-ublk: /bin/zx-virtio-blk is not in this image either' >&2; sleep 5; exit 127; }\n\
          exec /bin/zx-virtio-blk service \"$n\"\n",
    )
    .unwrap();
}

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
        let built = self.udrivers();
        for name in DRIVERS {
            put(&built.join(name), name);
        }
    }

    /// Build `tools/eclipse-udrivers` (Rust) as static, non-PIE musl
    /// binaries, like `eclipse-init`, and return the directory they are in.
    /// Best-effort: a binary that did not build is not there, and the
    /// service's wrapper says so on the boot that wanted it.
    fn udrivers(&self) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join(DRIVERS_PACKAGE);
        let triple = self.musl_rust_triple();
        let out = dir.join("target").join(triple).join("release");
        // Rebuild when a source file or the manifest is newer than any of
        // the binaries, or one is missing. The directory is the target's
        // own, so what is in it is never another architecture's.
        let newest = [
            newest_mtime_under(&dir.join("src")),
            fs::metadata(dir.join("Cargo.toml"))
                .and_then(|m| m.modified())
                .ok(),
        ]
        .into_iter()
        .flatten()
        .max();
        let fresh = |name: &str| {
            let built = fs::metadata(out.join(name)).and_then(|m| m.modified()).ok();
            matches!((built, newest), (Some(b), Some(s)) if b >= s)
        };
        if DRIVERS.into_iter().all(fresh) {
            return out;
        }

        println!("Compiling {DRIVERS_PACKAGE} (Rust, {triple})...");
        let _ = optional_status(
            Ext::new("rustup").arg("target").arg("add").arg(triple),
            "install rustup so the musl target can be added",
        );
        // A build that fails must not leave the last one to be copied in
        // its place.
        for name in DRIVERS {
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
            println!("Failed to compile {DRIVERS_PACKAGE}");
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
