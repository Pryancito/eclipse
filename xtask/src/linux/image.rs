use crate::{commands::wget, Arch, PROJECT_DIR, TARGET};
use os_xtask_utils::{dir, CommandExt, Qemu, Tar};
use std::{fs, path::Path};

/// ESP / primera partición (EFI). Debe coincidir con `PART1_SIZE_MIB` en
/// install-eclipse y `ESP_IMG_SIZE_MB` en el Makefile.
const EFI_PARTITION_BYTES: usize = 1024 * 1024 * 1024;

/// FAT32 ESP: siempre [`EFI_PARTITION_BYTES`]. Falla en build si los payloads
/// no caben. The bootstrap initramfs on the ESP is lean (LIVE_KEEP + GSP
/// firmware only — no desktop/LLVM), so 1 GiB is ample headroom for kernel
/// updates; it is not a dumping ground for `usr/lib`.
fn efi_fat_size_for(initramfs_bytes: u64, zcore_bytes: u64, boot_bytes: u64) -> usize {
    let payload = initramfs_bytes + zcore_bytes + boot_bytes;
    const FAT_METADATA_SLACK: u64 = 4 * 1024 * 1024;
    let max_payload = EFI_PARTITION_BYTES as u64 - FAT_METADATA_SLACK;
    assert!(
        payload <= max_payload,
        "EFI payloads ({} MiB) exceed the {} MiB the {} MiB ESP can hold \
         ({} MiB of it is FAT metadata slack) by {} MiB; \
         strip zcore or raise PART1_SIZE_MIB",
        payload.div_ceil(1024 * 1024),
        max_payload / (1024 * 1024),
        EFI_PARTITION_BYTES / (1024 * 1024),
        FAT_METADATA_SLACK / (1024 * 1024),
        (payload - max_payload).div_ceil(1024 * 1024),
    );
    EFI_PARTITION_BYTES
}

/// Installer payloads staged under the rootfs `/boot` for the live installer.
const BOOT_PAYLOADS: [&str; 3] = ["efi.img.gz", "rootfs.btrfs.gz", "home.btrfs.gz"];

/// Recursively sum the size (in bytes) of regular files under `path`.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    // A directory this cannot read used to be worth zero, silently, and every
    // image size below is computed from this number: the image then comes out
    // too small and `fuse` panics with `NoDeviceSpace`, a message that names
    // neither the directory nor the size.
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!(
                "warning: not counting {} towards the image size: {e}",
                path.display()
            );
            return 0;
        }
    };
    {
        for entry in entries.flatten() {
            let p = entry.path();
            let md = match fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.file_type().is_dir() {
                total += dir_size(&p);
            } else {
                total += md.len();
            }
        }
    }
    total
}

/// Bytes SFS will actually spend on the tree at `path`, which is what an SFS
/// image has to hold.
///
/// **Not** the sum of the file lengths -- that is what [`dir_size`] answers, and
/// sizing an SFS image from it is the bug this replaces. SFS spends a whole
/// block on each inode and then `ceil(len / BLKSIZE)` blocks on the contents, so
/// a tree of 3000 one-byte files is 3 KB of content and 24 MiB of image. The
/// percentage headroom below cannot cover the difference, because it is
/// proportional to BYTES while the shortfall is proportional to ENTRIES.
///
/// Measured on a tree of one 900 KiB file plus N symlinks:
/// `live_image_size(dir_size(..))` answered 17 MiB at N=1000 and 18 MiB at
/// N=5000 -- it barely moves -- and `fuse` panicked with `NoDeviceSpace` from
/// N=3000 on. Through this function the same tree sizes to 26 MiB and 61 MiB,
/// and every N fits.
fn sfs_payload_bytes(path: &Path) -> u64 {
    sfs_payload_blocks(path) * rcore_fs_sfs::BLKSIZE as u64
}

/// Blocks for the tree rooted at `path`, including the block for `path`'s own
/// inode. A symlink is charged like a file, because SFS stores its target in a
/// data block.
fn sfs_payload_blocks(path: &Path) -> u64 {
    const BLOCK: u64 = rcore_fs_sfs::BLKSIZE as u64;
    let mut blocks = 1;
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!(
                "warning: not counting {} towards the image size: {e}",
                path.display()
            );
            return blocks;
        }
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let md = match fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.file_type().is_dir() {
            blocks += sfs_payload_blocks(&p);
        } else {
            blocks += 1 + md.len().div_ceil(BLOCK);
        }
    }
    blocks
}

/// Round up to whole MiB, with `num/den` fractional headroom plus a `floor_mib`
/// absolute floor, for FS metadata (inode table, block bitmap, directories).
fn padded_image_size(payload_bytes: u64, num: u64, den: u64, floor_mib: u64) -> u64 {
    let with_slack = payload_bytes + payload_bytes * num / den + floor_mib * 1024 * 1024;
    let mib = with_slack.div_ceil(1024 * 1024);
    mib * 1024 * 1024
}

/// SFS image size for an initramfs holding `payload_bytes` (≈40% headroom).
fn sfs_size_for(payload_bytes: u64) -> usize {
    padded_image_size(payload_bytes, 2, 5, 24) as usize
}

/// SFS size for the *live* image (minimal root + installer payloads). The whole
/// SFS is loaded into RAM at boot, and the image is read-mostly (the installer
/// streams the gz payloads straight to the target disk), so it uses tight
/// headroom — 12.5% + 16 MiB — to avoid wasting RAM on the embedded payloads.
fn live_image_size(payload_bytes: u64) -> usize {
    padded_image_size(payload_bytes, 1, 8, 16) as usize
}

/// SFS size for the *QEMU* live image. Same tree as the ISO's plus the whole
/// desktop (`xorg::copy_into_live`), and -- unlike the installer media -- it is
/// the root a browser session actually WRITES to: `HOME=/root` is on it, so
/// Firefox's profile (startup cache, places.sqlite, session store), the
/// fontconfig caches and the wrapper logs all land here. With the lean
/// headroom (12.5 % plus 16 MiB) lunarbar showed `disk 95%` one minute after
/// boot and the image was full two minutes into Firefox (`unused_blocks: 0`).
/// 256 MiB of absolute floor covers a profile several times over; it costs
/// that much RAM once, since the SFS is loaded whole at boot (`-m 4G` on
/// x86_64).
fn qemu_live_image_size(payload_bytes: u64) -> usize {
    padded_image_size(payload_bytes, 1, 8, 256) as usize
}

/// Largest regular file copied into the minimal live root. Acts as a safety net:
/// a stray huge file (e.g. a `libLLVM.so` dropped into `/lib`) is left out so it
/// can't bloat the RAM-resident initramfs. Every installer essential (busybox,
/// apk, e2fsprogs, musl, install-eclipse, CA bundle) is comfortably under this.
const LIVE_FILE_CAP: u64 = 16 * 1024 * 1024;

/// Paths copied verbatim from the full rootfs into the minimal live root.
/// Everything else (X fonts in `usr/share/fonts`, libc-test, `perf`/`libLLVM`,
/// and any other heavy or user-added component) is intentionally omitted: it
/// ships in `rootfs.btrfs.gz` and runs from the btrfs disk on the installed
/// system, which pivots root onto it.
const LIVE_KEEP: [&str; 9] = [
    "bin",              // busybox + applets + install-eclipse + e2fsprogs + net tools + rc-*
    "lib",              // ld-musl + libeclipse_dns + apk db + OpenRC /lib/rc helpers (capped)
    "etc", // fstab, profile, ssl certs, apk repo, machine-id, X11, OpenRC (init.d/conf.d/runlevels/rc.conf)
    "var", // apk dbs (small)
    "sbin", // openrc-init / openrc / rc-* + /sbin/init -> openrc-init (INIT)
    "root", // root's home / rc files (capped)
    "usr/sbin", // openssl -> ssl_client wrapper
    "usr/share/udhcpc", // DHCP dispatcher scripts
    // Eclipse's own wrappers/helpers written by xtask desktop.rs:
    // eclipse-terminal, eclipse-firefox, labwc wrapper, eclipse-x11-prepare.
    // Without this the QEMU/live session lacks the X/XFCE first-boot cache
    // step and every desktop launcher that goes through a wrapper.
    "usr/local/bin",
];

/// Recursively copy `src` into `dst`, preserving symlinks (busybox applets are
/// symlinks to `busybox`) and permissions. Regular files larger than
/// [`LIVE_FILE_CAP`] are skipped. A missing `src` is a no-op.
fn copy_tree_capped(src: &Path, dst: &Path) {
    let md = match fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(_) => return,
    };
    if md.file_type().is_symlink() {
        let target = fs::read_link(src).unwrap();
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let _ = fs::remove_file(dst);
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dst).unwrap();
        return;
    }
    if md.is_dir() {
        fs::create_dir_all(dst).unwrap();
        for entry in fs::read_dir(src).unwrap().flatten() {
            copy_tree_capped(&entry.path(), &dst.join(entry.file_name()));
        }
        return;
    }
    if md.len() > LIVE_FILE_CAP {
        println!(
            "  live-rootfs: skipping large file {} ({} MiB)",
            src.display(),
            md.len() / (1024 * 1024)
        );
        return;
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::copy(src, dst).unwrap();
}

/// Build the *minimal live/installer* root at `out` from the full `full` rootfs.
/// Only the [`LIVE_KEEP`] paths are copied; empty mount points are created so
/// boot-time fstab processing and `/dev`, `/proc`, `/sys` have somewhere to
/// attach. The installer payloads are staged into `out/boot` by the caller.
fn build_live_rootfs(full: &Path, out: &Path) {
    let _ = fs::remove_dir_all(out);
    fs::create_dir_all(out).unwrap();
    for rel in LIVE_KEEP {
        copy_tree_capped(&full.join(rel), &out.join(rel));
    }
    for d in [
        "proc", "sys", "dev", "tmp", "run", "home", "boot", "boot/efi",
        // Xorg fatally aborts if /var/log is missing when it opens Xorg.0.log.
        "var/log",
    ] {
        let _ = fs::create_dir_all(out.join(d));
    }
}

impl super::LinuxRootfs {
    /// 生成镜像。
    pub fn image(&self) {
        // 递归 rootfs
        self.make(false);

        // For x86_64, build the installer images first
        if let Arch::X86_64 = self.0 {
            // The EFI image is assembled with external tools. Check for them
            // before the eight minutes of compiling below, because a missing
            // one surfaces as a bare unwrap on the spawn:
            //   called `Result::unwrap()` on an `Err` value:
            //   Os { code: 2, kind: NotFound, message: "No such file or directory" }
            // naming neither the tool nor the package it comes from. Same
            // checks, and the same advice, as the `iso` target in the root
            // Makefile. Only this arch builds an EFI image, which is why
            // aarch64 and riscv64 never needed them.
            for (tool, package) in [
                ("mkfs.vfat", "dosfstools"),
                ("mmd", "mtools"),
                ("mcopy", "mtools"),
            ] {
                let found = std::env::var_os("PATH").is_some_and(|paths| {
                    std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file())
                });
                assert!(found, "missing `{tool}` (package: {package})");
            }

            let rootfs_path = self.path();
            let boot_dir = rootfs_path.join("boot");
            fs::create_dir_all(&boot_dir).unwrap();

            // Real NVIDIA GSP-RM firmware for the vendored driver (see
            // nvidia-rm-sys) -- into the FULL rootfs only, never the
            // minimal live/installer root built below (LIVE_KEEP omits
            // lib/firmware entirely).
            super::nvidia_firmware::install(&rootfs_path);

            // Remove any installer payloads left over from a previous (possibly
            // failed) build. `make(false)` never clears the rootfs, so without
            // this the base initramfs / rootfs.btrfs below would be polluted
            // with stale efi.img.gz / *.btrfs.gz and overflow their images.
            for name in BOOT_PAYLOADS {
                let _ = fs::remove_file(boot_dir.join(name));
            }

            // Minimal root for the RAM-resident SFS images. The EFI bootstrap
            // (installed ESP) and the ISO installer initramfs are snapshotted
            // *before* the desktop stack is copied in: Mesa/LLVM/fonts stay in
            // `rootfs.btrfs.gz` (and, later, the QEMU live SFS). They must not
            // land in `\EFI\zCore\initramfs.img` or the ISO El Torito ESP.
            let live_root = TARGET.join("live-rootfs");
            println!("Building minimal live/installer root...");
            build_live_rootfs(&rootfs_path, &live_root);

            // The vendored NVIDIA driver reads gsp.bin at boot from the
            // initramfs (zCore/src/main.rs load_nvidia_gsp_firmware, right
            // after mounting the SFS root) -- BEFORE the full btrfs rootfs is
            // pivoted in. build_live_rootfs above copies `lib` but
            // copy_tree_capped drops gsp.bin for exceeding LIVE_FILE_CAP
            // (16 MiB < ~23 MiB), so the initramfs would lack it and
            // kgspInitRm could never run (confirmed on real hardware:
            // "lookup(/lib/firmware/nvidia/gsp/gsp.bin) failed: EntryNotFound"
            // at boot even though the mounted btrfs root has it). Install it
            // straight into the live root, uncapped, so it ships in the RAM
            // initramfs; sfs_size_for(sfs_payload_bytes(&live_root)) below sizes
            // the image to fit. This is the one heavy file deliberately kept
            // in the bootstrap initramfs -- the GPU needs its firmware at
            // bring-up time, not after root pivot. Copy the blob already
            // installed into the full rootfs (line above) rather than
            // re-fetching it.
            {
                let fw_rel = "lib/firmware/nvidia/gsp/gsp.bin";
                let fw_src = rootfs_path.join(fw_rel);
                if fw_src.is_file() {
                    let fw_dst = live_root.join(fw_rel);
                    if let Some(parent) = fw_dst.parent() {
                        fs::create_dir_all(parent).unwrap();
                    }
                    match fs::copy(&fw_src, &fw_dst) {
                        Ok(n) => println!(
                            "  live-rootfs: kept NVIDIA GSP firmware ({} MiB) for boot-time load",
                            n / (1024 * 1024)
                        ),
                        Err(e) => {
                            eprintln!("warning: could not copy GSP firmware into live root: {e}")
                        }
                    }
                }
            }

            // 1. Build bootloader (rboot)
            println!("Building bootloader (rboot)...");
            let rboot_dir = PROJECT_DIR.join("rboot");
            let status = std::process::Command::new("make")
                .arg("build")
                .current_dir(&rboot_dir)
                .status()
                .unwrap();
            assert!(status.success(), "Failed to build bootloader");

            // 2. Build kernel (zcore)
            println!("Building zCore kernel...");
            let build_config = crate::build::BuildConfig::from_args(crate::build::BuildArgs {
                machine: "virt-x86_64".to_string(),
                debug: false,
            });
            build_config.invoke(os_xtask_utils::Cargo::build);

            // 3. Bootstrap initramfs for the installed ESP. Snapshot NOW, while
            // live_root is still LIVE_KEEP + GSP only — no desktop, no LLVM,
            // no installer payloads. The installed system boots this only to
            // pivot onto btrfs. Written to a dedicated file so the later live
            // SFS can own zCore/x86_64.img (what `make qemu` copies to the ESP).
            let bootstrap_size = sfs_size_for(sfs_payload_bytes(&live_root));
            println!(
                "Building bootstrap initramfs.img ({} MiB, no desktop stack)...",
                bootstrap_size / (1024 * 1024)
            );
            let initramfs_img = TARGET.join("initramfs.img");
            fuse(&live_root, &initramfs_img, bootstrap_size);

            let rboot_efi = rboot_dir.join("target/x86_64-unknown-uefi/release/rboot.efi");
            let rboot_conf = PROJECT_DIR.join("zCore/rboot.conf");
            let zcore_elf = PROJECT_DIR.join("target/x86_64/release/zcore");

            // 4. Build efi.img (FAT32)
            let initramfs_len = fs::metadata(&initramfs_img).unwrap().len();
            let zcore_len = fs::metadata(&zcore_elf).unwrap().len();
            let boot_len =
                fs::metadata(&rboot_efi).unwrap().len() + fs::metadata(&rboot_conf).unwrap().len();
            let efi_fat_bytes = efi_fat_size_for(initramfs_len, zcore_len, boot_len);
            println!(
                "Building efi.img ({} MiB)...",
                efi_fat_bytes / (1024 * 1024)
            );
            let efi_img = TARGET.join("efi.img");
            let _ = fs::remove_file(&efi_img);

            let file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&efi_img)
                .unwrap();
            file.set_len(efi_fat_bytes as u64).unwrap();
            drop(file);

            let status = std::process::Command::new("mkfs.vfat")
                .arg("-F")
                .arg("32")
                .arg(&efi_img)
                .status()
                .unwrap();
            assert!(status.success(), "Failed to format efi.img");

            let status = std::process::Command::new("mmd")
                .arg("-i")
                .arg(&efi_img)
                .arg("::/EFI")
                .arg("::/EFI/Boot")
                .arg("::/EFI/zCore")
                .status()
                .unwrap();
            assert!(status.success(), "Failed to create EFI directories");

            let status = std::process::Command::new("mcopy")
                .arg("-i")
                .arg(&efi_img)
                .arg(&rboot_efi)
                .arg("::/EFI/Boot/BootX64.efi")
                .status()
                .unwrap();
            assert!(status.success(), "Failed to copy BootX64.efi");

            let status = std::process::Command::new("mcopy")
                .arg("-i")
                .arg(&efi_img)
                .arg(&rboot_conf)
                .arg("::/EFI/Boot/rboot.conf")
                .status()
                .unwrap();
            assert!(status.success(), "Failed to copy rboot.conf");

            let status = std::process::Command::new("mcopy")
                .arg("-i")
                .arg(&efi_img)
                .arg(&zcore_elf)
                .arg("::/EFI/zCore/zcore.elf")
                .status()
                .unwrap();
            assert!(status.success(), "Failed to copy zcore.elf");

            let status = std::process::Command::new("mcopy")
                .arg("-i")
                .arg(&efi_img)
                .arg(&initramfs_img)
                .arg("::/EFI/zCore/initramfs.img")
                .status()
                .unwrap();
            assert!(status.success(), "Failed to copy initramfs.img");

            println!("Compressing efi.img -> efi.img.gz...");
            let target_efi_gz = TARGET.join("efi.img.gz");
            let status = std::process::Command::new("gzip")
                .arg("-c")
                .arg(&efi_img)
                .stdout(fs::File::create(&target_efi_gz).unwrap())
                .status()
                .unwrap();
            assert!(status.success(), "Failed to compress efi.img");

            // NOTE: the payloads (efi.img.gz / rootfs.btrfs.gz / home.btrfs.gz)
            // are staged into the minimal live root's `/boot` only *after*
            // rootfs.btrfs is built (step 5c), so the full rootfs used for the
            // target root image below stays clean and does not contain the
            // installer's own payloads. The desktop copy into live_root happens
            // *after* the ISO installer SFS is fused, so Mesa never lands in
            // the ISO initramfs.

            // 5. Build rootfs.btrfs from the FULL rootfs (the installed system's
            // real root, reached by pivot). Size it to the actual rootfs with
            // generous headroom.
            println!("Building rootfs.btrfs...");
            let btrfs_img = TARGET.join("rootfs.btrfs");
            let rootfs_btrfs_size = std::cmp::max(
                96 * 1024 * 1024u64,
                padded_image_size(dir_size(&rootfs_path), 3, 5, 32),
            );
            super::btrfs_image::make_btrfs_image(
                &btrfs_img,
                rootfs_btrfs_size,
                "ECLIPSE",
                Some(&rootfs_path),
            );

            println!("Compressing rootfs.btrfs -> rootfs.btrfs.gz...");
            let target_btrfs_gz = TARGET.join("rootfs.btrfs.gz");
            let status = std::process::Command::new("gzip")
                .arg("-c")
                .arg(&btrfs_img)
                .stdout(fs::File::create(&target_btrfs_gz).unwrap())
                .status()
                .unwrap();
            assert!(status.success(), "Failed to compress rootfs.btrfs");

            // 5b. Empty btrfs template used by the installer to format HOME
            // (written raw onto the partition; the kernel auto-expands it).
            println!("Building home.btrfs template...");
            let home_img = TARGET.join("home.btrfs");
            super::btrfs_image::make_btrfs_image(&home_img, 32 * 1024 * 1024, "HOME", None);
            let target_home_gz = TARGET.join("home.btrfs.gz");
            let status = std::process::Command::new("gzip")
                .arg("-c")
                .arg(&home_img)
                .stdout(fs::File::create(&target_home_gz).unwrap())
                .status()
                .unwrap();
            assert!(status.success(), "Failed to compress home.btrfs");

            // 5c. Stage the payloads in the MINIMAL live root's /boot so the
            // live installer (which runs from this very initramfs) can find
            // them. They go into live_root — never the full rootfs — so
            // rootfs.btrfs.gz above is not polluted with the installer's own
            // payloads.
            let live_boot = live_root.join("boot");
            fs::create_dir_all(&live_boot).unwrap();
            fs::copy(&target_efi_gz, live_boot.join("efi.img.gz")).unwrap();
            fs::copy(&target_btrfs_gz, live_boot.join("rootfs.btrfs.gz")).unwrap();
            fs::copy(&target_home_gz, live_boot.join("home.btrfs.gz")).unwrap();

            // 5d. ISO installer SFS: LIVE_KEEP + GSP + payloads, no desktop.
            // `make iso` copies this to the El Torito ESP as initramfs.img so
            // the live session is console + install-eclipse; the desktop is
            // only inside rootfs.btrfs.gz (written to disk by the installer).
            let iso_size = live_image_size(sfs_payload_bytes(&live_root));
            println!(
                "Building ISO installer initramfs ({} MiB, no desktop stack)...",
                iso_size / (1024 * 1024)
            );
            let iso_initramfs = TARGET.join("iso-initramfs.img");
            fuse(&live_root, &iso_initramfs, iso_size);

            // Desktop stack belongs in the QEMU live initramfs only, never in
            // the EFI bootstrap or the ISO installer SFS just frozen above.
            // usr/bin + usr/lib (Mesa, libLLVM, fonts, icons) are omitted by
            // LIVE_KEEP; QEMU boots this tree without pivoting, so `startx`
            // still needs them here. Disable with ECLIPSE_XORG_LIVE=0 for a
            // lean QEMU image too.
            super::xorg::copy_into_live(&rootfs_path, &live_root);

            // 6. QEMU live SFS: installer payloads + desktop. Not written to
            // the installed ESP or the ISO — those carry the lean images from
            // steps 3 and 5d.
            let live_size = qemu_live_image_size(sfs_payload_bytes(&live_root));
            println!(
                "Building QEMU live image ({} MiB)...",
                live_size / (1024 * 1024)
            );
            let image = PROJECT_DIR
                .join("zCore")
                .join(format!("{arch}.img", arch = self.0.name()));
            fuse(&live_root, &image, live_size);

            println!("Build completed successfully!");
            return;
        }

        // 镜像路径
        let inner = PROJECT_DIR.join("zCore");
        let image = inner.join(format!("{arch}.img", arch = self.0.name()));
        // aarch64 还需要下载 firmware
        if let Arch::Aarch64 = self.0 {
            const URL: &str = "https://github.com/Luchangcheng2333/rayboot/releases/download/2.0.0/aarch64_firmware.tar.gz";
            let aarch64_tar = self.0.origin().join("Aarch64_firmware.zip");
            wget(URL, &aarch64_tar);

            let fw_dir = self.0.target().join("firmware");
            dir::clear(&fw_dir).unwrap();
            Tar::xf(&aarch64_tar, Some(&fw_dir)).invoke();

            let boot_dir = inner.join("disk").join("EFI").join("Boot");
            dir::clear(&boot_dir).unwrap();
            fs::copy(
                fw_dir.join("aarch64_uefi.efi"),
                boot_dir.join("bootaa64.efi"),
            )
            .unwrap();
            fs::copy(fw_dir.join("Boot.json"), boot_dir.join("Boot.json")).unwrap();
        }
        // 生成镜像
        fuse(
            self.path(),
            &image,
            sfs_size_for(sfs_payload_bytes(&self.path())),
        );
        // 扩充一些额外空间，供某些测试使用
        Qemu::img()
            .arg("resize")
            .args(["-f", "raw"])
            .arg(image)
            .arg("+5M")
            .invoke();
    }
}

/// DEBUG: repackear solo el initramfs SFS desde rootfs/x86_64 sin reconstruir
/// nada más. `cargo test -p xtask -- --ignored --nocapture dbg_repack_initramfs`.
///
/// `#[ignore]`: this is a developer helper, not a test. It needs a rootfs
/// that only `cargo rootfs` produces, so under plain `cargo test` (CI, a
/// fresh checkout) it failed with "No such file or directory" and turned the
/// whole workspace test run red.
#[test]
#[ignore = "debug helper: needs rootfs/x86_64 built by `cargo rootfs`; run with --ignored"]
fn dbg_repack_initramfs() {
    let rootfs = PROJECT_DIR.join("rootfs").join("x86_64");
    let image = PROJECT_DIR.join("zCore").join("x86_64.img");
    // Size dynamically like the production images do — the old fixed 80 MiB
    // made this helper useless the moment the rootfs grew past it.
    let size = sfs_size_for(sfs_payload_bytes(&rootfs));
    eprintln!(
        "repack {} ({} MiB) -> {} ({} MiB image)",
        rootfs.display(),
        dir_size(&rootfs) / (1024 * 1024),
        image.display(),
        size / (1024 * 1024),
    );
    fuse(&rootfs, &image, size);
    eprintln!("repack done");
}

/// 制作镜像。
fn fuse(dir: impl AsRef<Path>, image: impl AsRef<Path>, fs_size: usize) {
    use rcore_fs::vfs::FileSystem;
    use rcore_fs_fuse::zip::zip_dir;
    use rcore_fs_sfs::SimpleFileSystem;
    use std::sync::{Arc, Mutex};

    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(image)
        .expect("failed to open image");
    file.set_len(fs_size as u64)
        .expect("failed to set image size");
    let fs = SimpleFileSystem::create(Arc::new(Mutex::new(file)), fs_size)
        .expect("failed to create sfs");
    zip_dir(dir.as_ref(), fs.root_inode()).expect("failed to zip fs");
}

#[cfg(test)]
mod image_size_tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const BLOCK: u64 = rcore_fs_sfs::BLKSIZE as u64;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-image-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The whole point of sizing from blocks: an SFS image must hold what SFS
    /// actually spends, and a tree can be almost all metadata. Sized from the
    /// sum of the file lengths, a tree of one 900 KiB file plus 3000 symlinks
    /// came out at 18 MiB and `fuse` died with `NoDeviceSpace` -- a panic that
    /// names neither the size nor the tree. This is the case, fused for real.
    #[test]
    fn an_entry_heavy_tree_gets_an_image_that_actually_fits() {
        let dir = scratch("entry-heavy");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("busybox"), vec![0u8; 900 * 1024]).unwrap();
        for i in 0..3000 {
            std::os::unix::fs::symlink("busybox", bin.join(format!("applet{i}"))).unwrap();
        }

        let content = dir_size(&dir);
        let sized = live_image_size(sfs_payload_bytes(&dir));
        assert!(
            sized as u64 > content * 20,
            "3001 entries of {content} B of content need far more image than content: got {sized}"
        );
        // The image goes outside the tree: fuse creates it with set_len, so an
        // image inside the tree would be zipped into itself.
        let img = scratch("entry-heavy-img").join("live.img");
        fuse(&dir, &img, sized);
        assert!(fs::metadata(&img).unwrap().len() == sized as u64);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(img.parent().unwrap());
    }

    /// A one-byte file is one byte of content and TWO blocks of image: one for
    /// its inode and one for the byte. That factor of 8192 is the whole gap
    /// between the two questions, and the percentage headroom cannot close it
    /// because it is proportional to the content.
    #[test]
    fn a_one_byte_file_costs_two_blocks_and_not_one_byte() {
        let dir = scratch("one-byte");
        fs::write(dir.join("f"), b"x").unwrap();

        assert_eq!(dir_size(&dir), 1);
        // the directory's own inode, the file's inode, the file's one block
        assert_eq!(sfs_payload_bytes(&dir), 3 * BLOCK);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A symlink is charged like a file, because SFS keeps its target in a data
    /// block -- and `bin/` is hundreds of applet symlinks, which is exactly the
    /// tree that used to be under-counted into an image that would not hold it.
    #[test]
    fn a_symlink_is_charged_like_a_file() {
        let dir = scratch("symlink-cost");
        fs::write(dir.join("busybox"), b"x").unwrap();
        std::os::unix::fs::symlink("busybox", dir.join("ls")).unwrap();

        // directory inode + busybox (inode + block) + ls (inode + block)
        assert_eq!(sfs_payload_bytes(&dir), 5 * BLOCK);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An empty directory is not free, and a tree of empty directories is not
    /// worth zero: every inode is a block. `build_live_rootfs` creates nine
    /// empty mount points on purpose, so this is not a hypothetical shape.
    #[test]
    fn an_empty_directory_still_costs_its_own_inode() {
        let dir = scratch("empty");
        assert_eq!(dir_size(&dir), 0);
        assert_eq!(sfs_payload_bytes(&dir), BLOCK);

        for d in ["proc", "sys", "dev"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        assert_eq!(dir_size(&dir), 0);
        assert_eq!(sfs_payload_bytes(&dir), 4 * BLOCK);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Contents are charged by whole blocks, so one byte over a block boundary
    /// costs another whole block. Rounding the other way would under-size every
    /// image by a block per file.
    #[test]
    fn contents_are_charged_by_whole_blocks() {
        let dir = scratch("blocks");
        for (len, want_data_blocks) in [(0u64, 0u64), (1, 1), (BLOCK, 1), (BLOCK + 1, 2)] {
            let f = dir.join("f");
            fs::write(&f, vec![0u8; len as usize]).unwrap();
            assert_eq!(
                sfs_payload_bytes(&dir),
                (2 + want_data_blocks) * BLOCK,
                "a file of {len} bytes"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Whatever the payload, the answer is a whole number of MiB: the image is
    /// created with `set_len` and handed to SFS, and a partial MiB is a size no
    /// tool downstream (`qemu-img resize`, the ESP layout, the installer's
    /// partition arithmetic) expects.
    #[test]
    fn every_size_is_a_whole_number_of_mib() {
        for payload in [0u64, 1, 1023, MIB - 1, MIB, MIB + 1, 700 * MIB + 3] {
            for size in [
                sfs_size_for(payload) as u64,
                live_image_size(payload) as u64,
                qemu_live_image_size(payload) as u64,
            ] {
                assert_eq!(size % MIB, 0, "{size} for a payload of {payload}");
                assert!(size >= payload, "{size} cannot hold {payload}");
            }
        }
    }

    /// The rounding to whole MiB has to go UP. Rounding down would quietly eat
    /// up to a MiB of the headroom that was just computed, and the headroom is
    /// the only thing standing between the payload and `NoDeviceSpace`.
    #[test]
    fn the_rounding_goes_up_and_never_eats_the_headroom() {
        for payload in [1u64, 1023, MIB - 1, MIB + 1, 3 * MIB + 7, 700 * MIB + 3] {
            for (num, den, floor) in [(2u64, 5u64, 24u64), (1, 8, 16), (1, 8, 256)] {
                let asked = payload + payload * num / den + floor * MIB;
                let got = padded_image_size(payload, num, den, floor);
                assert!(
                    got >= asked,
                    "padded_image_size({payload}, {num}, {den}, {floor}) gave {got}, \
                     which is below the {asked} it was asked for"
                );
                assert!(
                    got - asked < MIB,
                    "it rounded up by more than a MiB: {got} vs {asked}"
                );
            }
        }
    }

    /// The three floors are load-bearing and each one is there for a reason the
    /// comments record. The 256 MiB of the QEMU image is the one that was paid
    /// for: with the lean 16 MiB floor the image filled two minutes into
    /// Firefox, because `HOME=/root` lives on it and the profile, the fontconfig
    /// caches and the wrapper logs all land there.
    #[test]
    fn the_floors_are_the_ones_the_comments_promise() {
        assert_eq!(
            sfs_size_for(0) as u64,
            24 * MIB,
            "bootstrap initramfs floor"
        );
        assert_eq!(live_image_size(0) as u64, 16 * MIB, "ISO installer floor");
        assert_eq!(
            qemu_live_image_size(0) as u64,
            256 * MIB,
            "the QEMU live image is the root a browser session writes to"
        );
    }

    /// And so are the percentages: 40% for the bootstrap initramfs, 12.5% for
    /// the two live images, which is what their comments say they trade RAM for.
    #[test]
    fn the_headroom_percentages_are_the_ones_the_comments_promise() {
        let payload = 800 * MIB;
        assert_eq!(
            sfs_size_for(payload) as u64,
            payload + payload * 2 / 5 + 24 * MIB
        );
        assert_eq!(
            live_image_size(payload) as u64,
            payload + payload / 8 + 16 * MIB
        );
        assert_eq!(
            qemu_live_image_size(payload) as u64,
            payload + payload / 8 + 256 * MIB
        );
    }

    /// The ESP is a fixed size, so the only thing this function can do when the
    /// payloads do not fit is stop the build with a message someone can act on.
    /// It used to report the PARTITION size where it meant the payload limit, so
    /// it could say "1022 MiB do not fit in the 1024 MiB ESP" -- which reads
    /// like a contradiction, in the one message that only ever appears when a
    /// build has already failed.
    #[test]
    #[should_panic(expected = "exceed the 1020 MiB the 1024 MiB ESP can hold")]
    fn the_esp_message_names_the_payload_limit_and_not_the_partition_size() {
        efi_fat_size_for(EFI_PARTITION_BYTES as u64, 0, 0);
    }

    /// And the boundary is where the slack says it is: the last byte that fits
    /// is accepted, the next one stops the build.
    #[test]
    fn the_esp_accepts_exactly_what_the_slack_leaves() {
        let max = EFI_PARTITION_BYTES as u64 - 4 * MIB;
        assert_eq!(efi_fat_size_for(max, 0, 0), EFI_PARTITION_BYTES);
        assert_eq!(
            efi_fat_size_for(max - 1, 1, 0),
            EFI_PARTITION_BYTES,
            "the three payloads are summed, not checked one by one"
        );
        assert!(std::panic::catch_unwind(|| efi_fat_size_for(max, 1, 0)).is_err());
    }

    /// `build_live_rootfs` decides what the RAM-resident installer root holds.
    /// Anything outside `LIVE_KEEP` ships in `rootfs.btrfs.gz` instead and must
    /// NOT be copied -- that is what keeps Mesa, the fonts and libLLVM out of an
    /// image that is loaded whole into memory at boot.
    #[test]
    fn the_live_root_keeps_only_what_live_keep_names() {
        let full = scratch("live-full");
        let out = scratch("live-out");
        for rel in ["bin", "usr/lib", "usr/share/fonts", "usr/bin"] {
            fs::create_dir_all(full.join(rel)).unwrap();
        }
        fs::write(full.join("bin/busybox"), b"busybox").unwrap();
        fs::write(full.join("usr/lib/libLLVM.so"), b"heavy").unwrap();
        fs::write(full.join("usr/share/fonts/DejaVu.ttf"), b"font").unwrap();
        fs::write(full.join("usr/bin/firefox"), b"browser").unwrap();

        build_live_rootfs(&full, &out);

        assert!(out.join("bin/busybox").is_file(), "busybox is an essential");
        for gone in [
            "usr/lib/libLLVM.so",
            "usr/share/fonts/DejaVu.ttf",
            "usr/bin/firefox",
        ] {
            assert!(
                !out.join(gone).exists(),
                "{gone} is not in LIVE_KEEP and would be loaded into RAM at boot"
            );
        }
        // The mount points have to exist for boot-time fstab processing, and
        // /var/log because Xorg aborts when it cannot open Xorg.0.log.
        for d in [
            "proc", "sys", "dev", "tmp", "run", "home", "boot", "boot/efi", "var/log",
        ] {
            assert!(out.join(d).is_dir(), "missing mount point {d}");
        }
        let _ = fs::remove_dir_all(&full);
        let _ = fs::remove_dir_all(&out);
    }

    /// Applet links must arrive as LINKS. Copying the target instead would put
    /// one busybox per applet into an image that is loaded whole into RAM --
    /// hundreds of copies of the same 900 KiB binary.
    #[test]
    fn a_symlink_arrives_as_a_symlink_and_not_as_a_copy() {
        let full = scratch("live-links-full");
        let out = scratch("live-links-out");
        fs::create_dir_all(full.join("bin")).unwrap();
        fs::write(full.join("bin/busybox"), vec![0u8; 4096]).unwrap();
        std::os::unix::fs::symlink("busybox", full.join("bin/ls")).unwrap();

        build_live_rootfs(&full, &out);

        let link = out.join("bin/ls");
        assert!(link.is_symlink(), "the applet link was copied, not linked");
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("busybox"));
        let _ = fs::remove_dir_all(&full);
        let _ = fs::remove_dir_all(&out);
    }

    /// The cap is the safety net against a stray heavy file bloating the
    /// RAM-resident image, and it applies to regular files only: a big file is
    /// left out, a small one comes through. (The GSP firmware is the one blob
    /// deliberately put back afterwards, uncapped, because the driver reads it
    /// before the real root is pivoted in.)
    #[test]
    fn a_file_over_the_cap_is_left_out() {
        let full = scratch("cap-full");
        let out = scratch("cap-out");
        fs::create_dir_all(full.join("bin")).unwrap();
        fs::write(full.join("bin/small"), vec![0u8; 1024]).unwrap();
        fs::write(full.join("bin/huge"), vec![0u8; LIVE_FILE_CAP as usize + 1]).unwrap();
        fs::write(full.join("bin/exactly"), vec![0u8; LIVE_FILE_CAP as usize]).unwrap();

        build_live_rootfs(&full, &out);

        assert!(out.join("bin/small").is_file());
        assert!(
            out.join("bin/exactly").is_file(),
            "the cap is a maximum, not an exclusive bound"
        );
        assert!(
            !out.join("bin/huge").exists(),
            "a file over the cap would be loaded into RAM at boot"
        );
        let _ = fs::remove_dir_all(&full);
        let _ = fs::remove_dir_all(&out);
    }

    /// The installer looks these three payloads up by name in its own `/boot`,
    /// so the list the build stages and the list the installer reads have to
    /// agree, and a duplicate would mean one of them is never staged.
    #[test]
    fn the_boot_payloads_are_distinct_gz_names() {
        let mut seen = std::collections::BTreeSet::new();
        for name in BOOT_PAYLOADS {
            assert!(seen.insert(name), "{name} is listed twice");
            assert!(name.ends_with(".gz"), "{name} is written out compressed");
            assert!(!name.contains('/'), "{name} is staged flat into /boot");
        }
    }

    /// `LIVE_KEEP` entries are relative paths joined onto both roots, so a
    /// leading slash would make the copy read and write the host's own
    /// directories instead of the two rootfs trees.
    #[test]
    fn the_live_keep_paths_are_relative_and_distinct() {
        let mut seen = std::collections::BTreeSet::new();
        for rel in LIVE_KEEP {
            assert!(seen.insert(rel), "{rel} is listed twice");
            assert!(
                Path::new(rel).is_relative(),
                "{rel} must be relative to the rootfs"
            );
        }
    }
}
