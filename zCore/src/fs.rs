cfg_if! {
    if #[cfg(feature = "linux")] {
        use alloc::sync::Arc;
        use rcore_fs::vfs::FileSystem;

        /// Where the libos rootfs is rooted: the repository, reached from the
        /// crate this binary was built in, or the working directory when the
        /// binary is run outside cargo.
        #[cfg(feature = "libos")]
        #[cfg_attr(feature = "zircon", allow(dead_code))]
        fn rootfs_dir() -> std::path::PathBuf {
            if let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") {
                std::path::Path::new(&dir).parent().unwrap().to_path_buf()
            } else {
                std::env::current_dir().unwrap()
            }
        }

        #[cfg(feature = "libos")]
        #[cfg_attr(feature = "zircon", allow(dead_code))]
        pub fn rootfs() -> Arc<dyn FileSystem> {
            rcore_fs_hostfs::HostFS::new(rootfs_dir().join("rootfs").join("libos"))
        }

        #[cfg(not(feature = "libos"))]
        pub fn rootfs() -> Arc<dyn FileSystem> {
            use rcore_fs::dev::Device;

            let device: Arc<dyn Device> = {
                #[cfg(feature = "mock-disk")]{
                    let block = linux_object::fs::mock_block();
                    Arc::new(block)
                }
                #[cfg(not(feature = "mock-disk"))] {
                    use linux_object::fs::rcore_fs_wrapper::*;
                    if let Some(initrd) = init_ram_disk() {
                        const SFS_MAGIC: u32 = 0x2f8dbe2b;
                        if initrd.len() >= 4 {
                            let magic = u32::from_le_bytes([
                                initrd[0], initrd[1], initrd[2], initrd[3],
                            ]);
                            if magic != SFS_MAGIC {
                                crate::klog_err!(
                                    "initramfs: magic {:#x} != SFS {:#x} (size={})",
                                    magic,
                                    SFS_MAGIC,
                                    initrd.len()
                                );
                            }
                        } else {
                            crate::klog_err!(
                                "initramfs demasiado pequeño para SFS (size={})",
                                initrd.len()
                            );
                        }
                        Arc::new(MemBuf::new(initrd))
                    } else {
                        let block = kernel_hal::drivers::all_block().first_unwrap();
                        Arc::new(BlockCache::new(Block::new(block), 0x100))
                    }
                }
            };
            crate::klog_info!("Eclipse: mounting root filesystem");
            rcore_fs_sfs::SimpleFileSystem::open(device).expect("failed to open device SimpleFS")
        }
    } else if #[cfg(feature = "zircon")] {

        #[cfg(feature = "libos")]
        pub fn zbi() -> impl AsRef<[u8]> {
            let path = std::env::args().nth(1).unwrap();
            std::fs::read(path).expect("failed to read zbi file")
        }

        #[cfg(not(feature = "libos"))]
        pub fn zbi() -> impl AsRef<[u8]> {
            init_ram_disk().expect("failed to get the init RAM disk")
        }
    }
}

#[cfg(not(feature = "libos"))]
#[cfg(feature = "link-user-img")]
#[allow(dead_code)]
pub(crate) fn init_ram_disk() -> Option<&'static mut [u8]> {
    Some(unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(USER_IMG.0).cast(), USER_IMG_LEN)
    })
}

#[cfg(all(not(feature = "libos"), not(feature = "link-user-img")))]
pub(crate) fn init_ram_disk() -> Option<&'static mut [u8]> {
    kernel_hal::boot::init_ram_disk()
}

// Embed the rootfs image in the kernel for platforms without an initrd handoff.
#[cfg(not(feature = "libos"))]
#[cfg(feature = "link-user-img")]
const USER_IMG_LEN: usize = include_bytes!(env!("USER_IMG")).len();

#[cfg(not(feature = "libos"))]
#[cfg(feature = "link-user-img")]
#[repr(align(4096))]
struct AlignedUserImage([u8; USER_IMG_LEN]);

#[cfg(not(feature = "libos"))]
#[cfg(feature = "link-user-img")]
#[link_section = ".data.img"]
static mut USER_IMG: AlignedUserImage = AlignedUserImage(*include_bytes!(env!("USER_IMG")));

#[cfg(all(test, feature = "linux", feature = "libos"))]
mod rootfs_tests {
    use super::rootfs_dir;

    /// The whole of the decision: `rootfs/libos` is a directory of the
    /// REPOSITORY and `CARGO_MANIFEST_DIR` names the crate -- `zCore/` -- so
    /// the path has to climb one level out of it. Without the climb the libos
    /// kernel mounts `zCore/rootfs/libos`, which does not exist, and comes up
    /// on an empty hostfs with no `/bin/sh` to run.
    #[test]
    fn the_rootfs_is_rooted_at_the_repository_not_at_this_crate() {
        // `env!` is this crate's directory at COMPILE time, which is what
        // cargo also puts in the environment when it runs the binary. Taking
        // it from there and putting it in means the test holds when the test
        // binary is started by hand too, which is how it gets run as an
        // unprivileged user here.
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let had_it = std::env::var_os("CARGO_MANIFEST_DIR").is_some();
        std::env::set_var("CARGO_MANIFEST_DIR", crate_dir);
        let found = rootfs_dir();
        if !had_it {
            std::env::remove_var("CARGO_MANIFEST_DIR");
        }
        assert_eq!(found, crate_dir.parent().unwrap());
        assert_ne!(found, crate_dir);
    }

    /// And with no cargo there is no crate to climb out of, so the working
    /// directory is the answer. That is the branch a person gets by running
    /// the built binary by hand, which is how the libos kernel is normally
    /// run.
    #[test]
    fn without_cargo_the_working_directory_is_the_root() {
        // Safe to take the variable away and give it back: this crate's suite
        // runs at `--test-threads=1`, and nothing else in the binary reads it.
        let saved = std::env::var("CARGO_MANIFEST_DIR").ok();
        std::env::remove_var("CARGO_MANIFEST_DIR");
        let found = rootfs_dir();
        if let Some(saved) = saved {
            std::env::set_var("CARGO_MANIFEST_DIR", saved);
        }
        assert_eq!(found, std::env::current_dir().unwrap());
    }
}
