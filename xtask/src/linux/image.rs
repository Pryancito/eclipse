use crate::{commands::wget, Arch, PROJECT_DIR, TARGET};
use os_xtask_utils::{dir, CommandExt, Tar};
use std::{fs, path::Path};

/// Fallback SFS initramfs size, only used by the `dbg_repack_initramfs` test.
/// Production images are sized dynamically: [`sfs_size_for`] for the minimal
/// efi-embedded bootstrap initramfs and [`live_image_size`] for the live image
/// that additionally embeds the installer payloads under `/boot`.
#[cfg(test)]
const INITRAMFS_BYTES: usize = 80 * 1024 * 1024;
/// FAT32 ESP image: initramfs + zcore (~40 MiB) + boot loader + metadata.
const EFI_FAT_BYTES: usize = 128 * 1024 * 1024;

/// Installer payloads staged under the rootfs `/boot` for the live installer.
const BOOT_PAYLOADS: [&str; 3] = ["efi.img.gz", "rootfs.btrfs.gz", "home.btrfs.gz"];

/// Recursively sum the size (in bytes) of regular files under `path`.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(path) {
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

/// Largest regular file copied into the minimal live root. Acts as a safety net:
/// a stray huge file (e.g. a `libLLVM.so` dropped into `/lib`) is left out so it
/// can't bloat the RAM-resident initramfs. Every installer essential (busybox,
/// apk, e2fsprogs, musl, install-eclipse, CA bundle) is comfortably under this.
const LIVE_FILE_CAP: u64 = 16 * 1024 * 1024;

/// Paths copied verbatim from the full rootfs into the minimal live root.
/// Everything else (TinyX/Xfbdev in `usr/bin`, X fonts in `usr/share/fonts`,
/// libc-test, `perf`/`libLLVM`, and any other heavy or user-added component)
/// is intentionally omitted: it ships in `rootfs.btrfs.gz` and runs from the
/// btrfs disk on the installed system, which pivots root onto it.
const LIVE_KEEP: [&str; 8] = [
    "bin",              // busybox + applets + install-eclipse + e2fsprogs + net tools
    "lib",              // ld-musl + libeclipse_dns + apk db (capped: drops stray big libs)
    "etc",              // fstab, profile, ssl certs, apk repo, machine-id, X11 configs
    "var",              // apk dbs (small)
    "sbin",             // openrc-init, if present (INIT)
    "root",             // root's home / rc files (capped)
    "usr/sbin",         // openssl -> ssl_client wrapper
    "usr/share/udhcpc", // DHCP dispatcher scripts
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

/// TinyX paths pulled into the live image on top of [`LIVE_KEEP`]. The bulk of
/// `usr/bin` and `usr/share/fonts` is intentionally left out of the RAM-resident
/// live root, but `make qemu` boots that live image directly (never installs to
/// btrfs), so without these the X server isn't reachable there. Xfbdev is ~2 MB
/// and the misc bitmap fonts ~5 MB — negligible for a 2 GiB VM, and copying the
/// whole `misc` dir avoids a fragile curated subset where the `fixed`/`cursor`
/// aliases could resolve to an omitted font and Xfbdev would fail to start.
/// `etc/X11/xinitrc.tinyx` rides along via the `etc` entry in [`LIVE_KEEP`].
const LIVE_TINYX: [&str; 3] = [
    "usr/bin/Xfbdev",
    "usr/bin/startx",
    "usr/share/fonts/X11/misc",
];

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
    // TinyX (Xfbdev) so `make qemu`, which boots the live image, has an X server.
    for rel in LIVE_TINYX {
        copy_tree_capped(&full.join(rel), &out.join(rel));
    }
    for d in [
        "proc", "sys", "dev", "tmp", "run", "home", "boot", "boot/efi",
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
            let rootfs_path = self.path();
            let boot_dir = rootfs_path.join("boot");
            fs::create_dir_all(&boot_dir).unwrap();

            // Remove any installer payloads left over from a previous (possibly
            // failed) build. `make(false)` never clears the rootfs, so without
            // this the base initramfs / rootfs.btrfs below would be polluted
            // with stale efi.img.gz / *.btrfs.gz and overflow their images.
            for name in BOOT_PAYLOADS {
                let _ = fs::remove_file(boot_dir.join(name));
            }

            // Build the minimal live/installer root. Both initramfs images (the
            // efi-embedded bootstrap and the live image) are built from THIS,
            // not from the full rootfs: they are loaded into RAM at boot, so the
            // heavy OS components (TinyX, perf, libLLVM, libc-test, …) must stay
            // out of them. Those ship only in rootfs.btrfs.gz and run from the
            // btrfs disk on the installed system (which pivots root onto it).
            let live_root = TARGET.join("live-rootfs");
            println!("Building minimal live/installer root...");
            build_live_rootfs(&rootfs_path, &live_root);

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

            // 3. Build the efi-embedded bootstrap initramfs SFS (x86_64.img)
            // from the minimal root (no payloads). This is the initramfs the
            // installer writes (inside efi.img) to the target ESP; the installed
            // system boots it only to pivot root onto the btrfs partition.
            let bootstrap_size = sfs_size_for(dir_size(&live_root));
            println!(
                "Building bootstrap initramfs x86_64.img ({} MiB)...",
                bootstrap_size / (1024 * 1024)
            );
            let initramfs_img = PROJECT_DIR.join("zCore").join("x86_64.img");
            fuse(&live_root, &initramfs_img, bootstrap_size);

            // 4. Build efi.img (FAT32)
            println!("Building efi.img...");
            let efi_img = TARGET.join("efi.img");
            let _ = fs::remove_file(&efi_img);

            let file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&efi_img)
                .unwrap();
            file.set_len(EFI_FAT_BYTES as u64).unwrap();
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

            let rboot_efi = rboot_dir.join("target/x86_64-unknown-uefi/release/rboot.efi");
            let rboot_conf = PROJECT_DIR.join("zCore/rboot.conf");
            let zcore_elf = PROJECT_DIR.join("target/x86_64/release/zcore");

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
            // installer's own payloads.

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

            // 6. Build the final installer-enabled x86_64.img (SFS) for QEMU/ESP
            // from the minimal live root + payloads. Sized tightly because the
            // whole image is loaded into RAM at boot.
            let live_size = live_image_size(dir_size(&live_root));
            println!(
                "Building final installer-enabled image ({} MiB)...",
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
            const URL:&str = "https://github.com/Luchangcheng2333/rayboot/releases/download/2.0.0/aarch64_firmware.tar.gz";
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
        fuse(self.path(), &image, 48 * 1024 * 1024);
    }
}

/// DEBUG: repackear solo el initramfs SFS desde rootfs/x86_64 sin reconstruir
/// nada más. `cargo test -p xtask -- --nocapture dbg_repack_initramfs`.
#[test]
fn dbg_repack_initramfs() {
    let rootfs = PROJECT_DIR.join("rootfs").join("x86_64");
    let image = PROJECT_DIR.join("zCore").join("x86_64.img");
    eprintln!("repack {} -> {}", rootfs.display(), image.display());
    fuse(&rootfs, &image, INITRAMFS_BYTES);
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
