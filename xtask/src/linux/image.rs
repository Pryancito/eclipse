use crate::{commands::wget, Arch, PROJECT_DIR, TARGET};
use os_xtask_utils::{dir, CommandExt, Tar};
use std::{fs, path::Path};

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

            // 3. Build clean x86_64.img (SFS, 48MB)
            println!("Building clean_x86_64.img...");
            let clean_img = TARGET.join("clean_x86_64.img");
            fuse(&rootfs_path, &clean_img, 48 * 1024 * 1024);

            // 4. Build efi.img (FAT32, 100MB)
            println!("Building efi.img...");
            let efi_img = TARGET.join("efi.img");
            let _ = fs::remove_file(&efi_img);
            
            let file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&efi_img)
                .unwrap();
            file.set_len(100 * 1024 * 1024).unwrap();
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
                .arg(&clean_img)
                .arg("::/EFI/zCore/initramfs.img")
                .status()
                .unwrap();
            assert!(status.success(), "Failed to copy initramfs.img");

            println!("Compressing efi.img -> efi.img.gz...");
            let efi_gz = boot_dir.join("efi.img.gz");
            let status = std::process::Command::new("gzip")
                .arg("-c")
                .arg(&efi_img)
                .stdout(fs::File::create(&efi_gz).unwrap())
                .status()
                .unwrap();
            assert!(status.success(), "Failed to compress efi.img");

            // 5. Build rootfs.ext2
            println!("Building rootfs.ext2...");
            let ext2_img = TARGET.join("rootfs.ext2");
            let _ = fs::remove_file(&ext2_img);
            
            let file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&ext2_img)
                .unwrap();
            file.set_len(48 * 1024 * 1024).unwrap();
            drop(file);

            let status = std::process::Command::new("mke2fs")
                .arg("-t")
                .arg("ext2")
                .arg("-d")
                .arg(&rootfs_path)
                .arg(&ext2_img)
                .status()
                .unwrap();
            assert!(status.success(), "Failed to format/populate rootfs.ext2");

            println!("Compressing rootfs.ext2 -> rootfs.ext2.gz...");
            let ext2_gz = boot_dir.join("rootfs.ext2.gz");
            let status = std::process::Command::new("gzip")
                .arg("-c")
                .arg(&ext2_img)
                .stdout(fs::File::create(&ext2_gz).unwrap())
                .status()
                .unwrap();
            assert!(status.success(), "Failed to compress rootfs.ext2");

            // 6. Build the final installer-enabled x86_64.img (SFS, 80MB)
            println!("Building final installer-enabled image...");
            let image = PROJECT_DIR.join("zCore").join(format!("{arch}.img", arch = self.0.name()));
            fuse(&rootfs_path, &image, 80 * 1024 * 1024);

            let _ = fs::remove_file(efi_gz);
            let _ = fs::remove_file(ext2_gz);
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
    file.set_len(fs_size as u64).expect("failed to set image size");
    let fs = SimpleFileSystem::create(Arc::new(Mutex::new(file)), fs_size)
        .expect("failed to create sfs");
    zip_dir(dir.as_ref(), fs.root_inode()).expect("failed to zip fs");
}

