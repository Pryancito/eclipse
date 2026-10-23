//! The ioctls, xattrs, fallocate and reflink on RAM disks, through the
//! same `Target` the syscall reaches, with the argument structs laid out
//! byte for byte as btrfs-progs and coreutils lay them out.

use super::super::eclipsefs_mount::testdisk::*;
use super::super::eclipsefs_mount::{forget_block_device, register_block_device, EfsOptions};
use super::super::OpenFlags;
use super::*;
use alloc::format;
use alloc::string::ToString;
use lock::Mutex;
use rcore_fs::vfs::{FileSystem, FileType, FsError};
use zcore_drivers::scheme::BlockScheme;

/// The process side: its descriptors (by index), credentials and the
/// device nodes it can name.
struct Ctx {
    files: Mutex<Vec<Arc<File>>>,
    euid: u32,
    devices: Vec<(String, Arc<dyn INode>)>,
}

impl Ctx {
    fn root() -> Self {
        Self::as_user(0)
    }

    fn as_user(euid: u32) -> Self {
        Self {
            files: Mutex::new(Vec::new()),
            euid,
            devices: Vec::new(),
        }
    }

    /// Open `inode` and return its descriptor.
    fn open(&self, inode: Arc<dyn INode>, flags: OpenFlags) -> i32 {
        let mut files = self.files.lock();
        files.push(File::new(inode, flags, String::from("/efs-test/f")));
        files.len() as i32 - 1
    }

    fn fd(&self, fd: i32) -> Arc<File> {
        self.files.lock()[fd as usize].clone()
    }

    /// Run `cmd` on descriptor `fd` with `buf` as its argument.
    fn ioctl(&self, fd: i32, cmd: u32, buf: &mut [u8]) -> LxResult<usize> {
        let file = self.fd(fd);
        Target::of(&file)
            .expect("an EclipseFS file")
            .ioctl(cmd, buf, self)
    }
}

impl FsIoctlContext for Ctx {
    fn file(&self, fd: i32) -> LxResult<Arc<File>> {
        self.files
            .lock()
            .get(fd as usize)
            .cloned()
            .ok_or(LxError::EBADF)
    }
    fn lookup(&self, path: &str) -> LxResult<Arc<dyn INode>> {
        self.devices
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, i)| i.clone())
            .ok_or(LxError::ENOENT)
    }
    fn capable(&self, _cap: u32) -> bool {
        self.euid == 0
    }
    fn euid(&self) -> u32 {
        self.euid
    }
}

fn mount(disk: &Arc<RamDisk>) -> Arc<EclipseMountFs> {
    mount_with(disk, false, "")
}

fn mount_with(disk: &Arc<RamDisk>, read_only: bool, data: &str) -> Arc<EclipseMountFs> {
    let opts = EfsOptions::parse(data).unwrap();
    EclipseMountFs::open_with(&MountBackend::Block(disk.clone()), read_only, &opts).unwrap()
}

fn file_with(dir: &Arc<dyn INode>, name: &str, data: &[u8]) -> Arc<dyn INode> {
    let f = dir.create(name, FileType::File, 0o644).unwrap();
    if !data.is_empty() {
        assert_eq!(f.write_at(0, data).unwrap(), data.len());
    }
    f
}

fn read_all(f: &Arc<dyn INode>) -> Vec<u8> {
    let size = f.metadata().unwrap().size;
    let mut buf = vec![0u8; size];
    let mut done = 0;
    while done < size {
        done += f.read_at(done, &mut buf[done..]).unwrap();
    }
    buf
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// `struct btrfs_ioctl_vol_args`: an fd and a name.
fn vol_args(fd: i64, name: &str) -> Vec<u8> {
    let mut b = vec![0u8; 4096];
    b[0..8].copy_from_slice(&fd.to_le_bytes());
    b[8..8 + name.len()].copy_from_slice(name.as_bytes());
    b
}

/// `struct btrfs_ioctl_vol_args_v2`.
fn vol_args_v2(fd: i64, flags: u64, name: &str) -> Vec<u8> {
    let mut b = vec![0u8; 4096];
    b[0..8].copy_from_slice(&fd.to_le_bytes());
    b[16..24].copy_from_slice(&flags.to_le_bytes());
    b[56..56 + name.len()].copy_from_slice(name.as_bytes());
    b
}

fn used(fs: &Arc<EclipseMountFs>) -> u64 {
    fs.inner.lock().fsinfo().bytes_used
}

fn names(dir: &Arc<dyn INode>) -> Vec<String> {
    (2..)
        .map_while(|i| dir.get_entry(i).ok())
        .collect::<Vec<_>>()
}

#[test]
fn the_ioctl_numbers_are_the_ones_linux_and_btrfs_progs_use() {
    const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
        dir << 30 | size << 16 | ty << 8 | nr
    }
    const W: u32 = 1;
    const R: u32 = 2;
    assert_eq!(FS_IOC_GETFLAGS, ioc(R, b'f' as u32, 1, 8));
    assert_eq!(FS_IOC_SETFLAGS, ioc(W, b'f' as u32, 2, 8));
    assert_eq!(FICLONE, ioc(W, 0x94, 9, 4));
    assert_eq!(FICLONERANGE, ioc(W, 0x94, 13, 32));
    assert_eq!(FIDEDUPERANGE, ioc(R | W, 0x94, 54, 24));
    assert_eq!(FS_IOC_FIEMAP, ioc(R | W, b'f' as u32, 11, 32));
    assert_eq!(BTRFS_IOC_DEFRAG, ioc(W, 0x94, 2, 4096));
    assert_eq!(BTRFS_IOC_RESIZE, ioc(W, 0x94, 3, 4096));
    assert_eq!(BTRFS_IOC_DEFRAG_RANGE, ioc(W, 0x94, 16, 48));
    assert_eq!(BTRFS_IOC_SNAP_CREATE_V2, ioc(W, 0x94, 23, 4096));
    assert_eq!(BTRFS_IOC_SUBVOL_CREATE_V2, ioc(W, 0x94, 24, 4096));
    assert_eq!(BTRFS_IOC_START_SYNC, ioc(R, 0x94, 24, 8));
    assert_eq!(BTRFS_IOC_SCRUB, ioc(R | W, 0x94, 27, 1024));
    assert_eq!(BTRFS_IOC_DEV_INFO, ioc(R | W, 0x94, 30, 4096));
    assert_eq!(BTRFS_IOC_FS_INFO, ioc(R, 0x94, 31, 1024));
    assert_eq!(BTRFS_IOC_BALANCE_V2, ioc(R | W, 0x94, 32, 1024));
    assert_eq!(BTRFS_IOC_SEND, ioc(W, 0x94, 38, 72));
    assert_eq!(BTRFS_IOC_QGROUP_LIMIT, ioc(R, 0x94, 43, 48));
    assert_eq!(BTRFS_IOC_DEV_REPLACE, ioc(R | W, 0x94, 53, 2600));
    assert_eq!(BTRFS_IOC_GET_SUBVOL_INFO, ioc(R, 0x94, 60, 504));
    assert_eq!(BTRFS_IOC_SNAP_DESTROY_V2, ioc(W, 0x94, 63, 4096));
    assert_eq!(EFS_IOC_RECEIVE, ioc(W, 0x94, 0xf0, 32));
    assert_eq!(EFS_IOC_SET_TIER, ioc(W, 0x94, 0xf1, 32));
    assert_eq!(EFS_IOC_GET_TIER, ioc(R | W, 0x94, 0xf2, 32));
    assert_eq!(EFS_IOC_TIER_MIGRATE, ioc(R | W, 0x94, 0xf3, 32));
    assert!(is_fs_ioctl(FICLONE));
    assert!(!is_fs_ioctl(0x5401), "TCGETS is not ours");
    assert!(is_vfs_clone_ioctl(FICLONE));
    assert!(is_vfs_clone_ioctl(FICLONERANGE));
    assert!(is_vfs_clone_ioctl(FIDEDUPERANGE));
    assert!(!is_vfs_clone_ioctl(FS_IOC_GETFLAGS));
}

/// The request numbers the test above does not reach, same derivation.
///
/// That test grew with the dispatch arms it was written beside and stopped
/// at 29 of the 82 numbers this module carries. The other 53 are no less
/// a wire contract: every one of them is chosen by `btrfs-progs`,
/// `chattr`, `filefrag` or `fscryptctl` out of its own copy of these
/// headers, and nothing inside this kernel can notice a wrong one. Both
/// halves of a dispatch read the same constant and agree with each other
/// whatever it says; the request simply falls past its arm to the
/// `ENOTTY` at the end, which the caller reads as "this filesystem does
/// not do that". `btrfs subvolume snapshot` answering that is a backup
/// that never runs.
///
/// So these are built the way `include/uapi/asm-generic/ioctl.h` builds
/// them, out of the magic letter and the command number written beside
/// each name in `linux/btrfs.h` and `linux/fs.h`, and can be read against
/// those headers line by line.
#[test]
fn the_rest_of_the_ioctl_numbers_are_the_ones_linux_uses() {
    const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
        dir << 30 | size << 16 | ty << 8 | nr
    }
    const NONE: u32 = 0;
    const W: u32 = 1;
    const R: u32 = 2;
    assert_eq!(FS_IOC32_GETFLAGS, ioc(R, b'f' as u32, 1, 4));
    assert_eq!(FS_IOC32_SETFLAGS, ioc(W, b'f' as u32, 2, 4));
    assert_eq!(BTRFS_IOC_SNAP_CREATE, ioc(W, 0x94, 1, 4096));
    assert_eq!(BTRFS_IOC_SYNC, ioc(NONE, 0x94, 8, 0));
    assert_eq!(BTRFS_IOC_ADD_DEV, ioc(W, 0x94, 10, 4096));
    assert_eq!(BTRFS_IOC_RM_DEV, ioc(W, 0x94, 11, 4096));
    assert_eq!(BTRFS_IOC_SUBVOL_CREATE, ioc(W, 0x94, 14, 4096));
    assert_eq!(BTRFS_IOC_SNAP_DESTROY, ioc(W, 0x94, 15, 4096));
    assert_eq!(BTRFS_IOC_INO_LOOKUP, ioc(R | W, 0x94, 18, 4096));
    assert_eq!(BTRFS_IOC_DEFAULT_SUBVOL, ioc(W, 0x94, 19, 8));
    assert_eq!(BTRFS_IOC_WAIT_SYNC, ioc(W, 0x94, 22, 8));
    assert_eq!(BTRFS_IOC_SUBVOL_GETFLAGS, ioc(R, 0x94, 25, 8));
    assert_eq!(BTRFS_IOC_SUBVOL_SETFLAGS, ioc(W, 0x94, 26, 8));
    assert_eq!(BTRFS_IOC_SCRUB_CANCEL, ioc(NONE, 0x94, 28, 0));
    assert_eq!(BTRFS_IOC_SCRUB_PROGRESS, ioc(R | W, 0x94, 29, 1024));
    assert_eq!(BTRFS_IOC_BALANCE_CTL, ioc(W, 0x94, 33, 4));
    assert_eq!(BTRFS_IOC_BALANCE_PROGRESS, ioc(R, 0x94, 34, 1024));
    assert_eq!(BTRFS_IOC_QUOTA_CTL, ioc(R | W, 0x94, 40, 16));
    assert_eq!(BTRFS_IOC_QUOTA_RESCAN, ioc(W, 0x94, 44, 64));
    assert_eq!(BTRFS_IOC_QUOTA_RESCAN_STATUS, ioc(R, 0x94, 45, 64));
    assert_eq!(BTRFS_IOC_QUOTA_RESCAN_WAIT, ioc(NONE, 0x94, 46, 0));
    assert_eq!(BTRFS_IOC_GET_FEATURES, ioc(R, 0x94, 57, 24));
    assert_eq!(BTRFS_IOC_RM_DEV_V2, ioc(W, 0x94, 58, 4096));
}

/// The flag bits and status codes, which are wire numbers just the same.
///
/// These are not request numbers, so a wrong one does not fall through to
/// `ENOTTY`: it is obeyed, as a different flag. `BTRFS_SUBVOL_RDONLY`
/// with the bit of `BTRFS_SUBVOL_QGROUP_INHERIT` makes
/// `btrfs subvolume snapshot -r` hand back a writable snapshot, and a
/// wrong `BTRFS_BLOCK_GROUP_RAID1` makes `btrfs filesystem usage` name
/// the wrong profile for the data it is about to rebalance.
///
/// Written from `linux/btrfs.h`, `linux/btrfs_tree.h`, `linux/fs.h`,
/// `linux/fiemap.h`, `linux/fscrypt.h` and `linux/capability.h`, as plain
/// numbers rather than as the shifts the declarations use, so that the
/// two spellings have to agree.
#[test]
fn the_flag_bits_and_status_codes_are_the_ones_userspace_names() {
    // Subvolume and device arguments.
    assert_eq!(BTRFS_SUBVOL_RDONLY, 0x2);
    assert_eq!(BTRFS_SUBVOL_QGROUP_INHERIT, 0x4);
    assert_eq!(BTRFS_DEVICE_SPEC_BY_ID, 0x8);
    assert_eq!(BTRFS_SUBVOL_SPEC_BY_ID, 0x10);
    // What `BTRFS_IOC_FS_INFO` says it filled in.
    assert_eq!(BTRFS_FS_INFO_FLAG_CSUM_INFO, 0x1);
    assert_eq!(BTRFS_FS_INFO_FLAG_GENERATION, 0x2);
    assert_eq!(BTRFS_FS_INFO_FLAG_METADATA_UUID, 0x4);
    // Balance, defrag, quota and send.
    assert_eq!(BTRFS_BALANCE_DATA, 0x1);
    assert_eq!(BTRFS_BALANCE_SYSTEM, 0x2);
    assert_eq!(BTRFS_BALANCE_METADATA, 0x4);
    assert_eq!(BTRFS_BALANCE_RESUME, 0x10);
    assert_eq!(BTRFS_BALANCE_ARGS_CONVERT, 0x100);
    assert_eq!(BTRFS_DEFRAG_RANGE_COMPRESS, 0x1);
    assert_eq!(BTRFS_DEFRAG_RANGE_START_IO, 0x2);
    assert_eq!(BTRFS_QGROUP_LIMIT_MAX_RFER, 0x1);
    assert_eq!(BTRFS_SEND_FLAG_VERSION, 0x8);
    // The allocation profiles, which go out in `fs_info` and in the
    // balance arguments.
    assert_eq!(BTRFS_BLOCK_GROUP_RAID0, 0x8);
    assert_eq!(BTRFS_BLOCK_GROUP_RAID1, 0x10);
    assert_eq!(BTRFS_BLOCK_GROUP_DUP, 0x20);
    assert_eq!(BTRFS_BLOCK_GROUP_RAID10, 0x40);
    assert_eq!(BTRFS_BLOCK_GROUP_RAID5, 0x80);
    assert_eq!(BTRFS_BLOCK_GROUP_RAID6, 0x100);
    assert_eq!(BTRFS_BLOCK_GROUP_RAID1C3, 0x200);
    assert_eq!(BTRFS_BLOCK_GROUP_RAID1C4, 0x400);
    assert_eq!(BTRFS_AVAIL_ALLOC_BIT_SINGLE, 0x1_0000_0000_0000);
    // `chattr` and `lsattr`, and the one `filefrag` sets.
    assert_eq!(FS_IMMUTABLE_FL, 0x10);
    assert_eq!(FS_APPEND_FL, 0x20);
    assert_eq!(FIEMAP_FLAG_SYNC, 0x1);
    // What `FIDEDUPERANGE` writes back per range.
    assert_eq!(FILE_DEDUPE_RANGE_SAME, 0);
    assert_eq!(FILE_DEDUPE_RANGE_DIFFERS, 1);
    // `fscryptctl`: the policy version, the key specifier it uses, and
    // the three status words a key query answers with.
    assert_eq!(FSCRYPT_POLICY_V2, 2);
    assert_eq!(FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER, 2);
    assert_eq!(FSCRYPT_KEY_STATUS_ABSENT, 1);
    assert_eq!(FSCRYPT_KEY_STATUS_PRESENT, 2);
    assert_eq!(FSCRYPT_KEY_STATUS_FLAG_ADDED_BY_SELF, 1);
    // The three capabilities these ioctls check, from `capability.h`.
    assert_eq!(CAP_FOWNER, 3);
    assert_eq!(CAP_LINUX_IMMUTABLE, 9);
    assert_eq!(CAP_SYS_ADMIN, 21);
}

/// The guard that makes the list above have to grow.
///
/// A list of `const`s is not an enum and not a `bitflags`, so a member
/// added without a line in the test above cannot be made to fail to
/// compile. What it can be made to do is fail here: Linux publishes the
/// or of each family as a mask of its own, and a new profile or balance
/// type changes the mask without changing the sum of what is named.
#[test]
fn no_profile_or_balance_type_is_missing_from_the_list_above() {
    // BTRFS_BLOCK_GROUP_PROFILE_MASK, btrfs_tree.h.
    assert_eq!(
        BTRFS_BLOCK_GROUP_RAID0
            | BTRFS_BLOCK_GROUP_RAID1
            | BTRFS_BLOCK_GROUP_DUP
            | BTRFS_BLOCK_GROUP_RAID10
            | BTRFS_BLOCK_GROUP_RAID5
            | BTRFS_BLOCK_GROUP_RAID6
            | BTRFS_BLOCK_GROUP_RAID1C3
            | BTRFS_BLOCK_GROUP_RAID1C4,
        0x7f8,
        "the profiles named here are not BTRFS_BLOCK_GROUP_PROFILE_MASK"
    );
    // BTRFS_EXTENDED_PROFILE_MASK: the above plus the single-copy bit.
    assert_eq!(
        0x7f8_u64 | BTRFS_AVAIL_ALLOC_BIT_SINGLE,
        0x1_0000_0000_07f8,
        "the extended profile mask is not what btrfs_tree.h lays out"
    );
    // BTRFS_BALANCE_TYPE_MASK, btrfs.h.
    assert_eq!(
        BTRFS_BALANCE_DATA | BTRFS_BALANCE_SYSTEM | BTRFS_BALANCE_METADATA,
        0x7,
        "the balance types named here are not BTRFS_BALANCE_TYPE_MASK"
    );
}

/// The struct sizes these ioctls measure their buffers against.
///
/// Each one is a `sizeof` in the header, and it is also baked into the
/// request number above as its 14-bit size field, so a wrong one here is
/// a short read or a buffer walked off the end rather than a flag obeyed
/// wrongly. They are spelled out from the layouts in `linux/fiemap.h` and
/// `linux/fscrypt.h`.
#[test]
fn the_struct_sizes_the_ioctls_measure_against_are_the_ones_linux_lays_out() {
    // struct fiemap: two u64, four u32, then the extent array.
    assert_eq!(FIEMAP_HEADER, 8 + 8 + 4 + 4 + 4 + 4);
    // struct fiemap_extent: three u64, two reserved u64, a u32 of flags
    // and three reserved u32.
    assert_eq!(FIEMAP_EXTENT, 8 + 8 + 8 + 16 + 4 + 12);
    // struct fscrypt_policy_v2: eight bytes of header and the 16-byte
    // master key identifier.
    assert_eq!(FSCRYPT_POLICY_V2_SIZE, 8 + 16);
    // struct fscrypt_key_specifier: a u32 type, a u32 of padding and a
    // 32-byte union.
    assert_eq!(FSCRYPT_KEY_SPEC, 4 + 4 + 32);
    // struct fscrypt_add_key_arg: that specifier, two u32 and eight
    // reserved u32, with the raw key following it.
    assert_eq!(FSCRYPT_ADD_KEY_ARG, 40 + 4 + 4 + 32);
    assert_eq!(FSCRYPT_MAX_KEY_SIZE, 64);
}

#[test]
fn an_ioctl_this_module_does_not_know_is_enotty() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let ctx = Ctx::root();
    let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
    assert_eq!(ctx.ioctl(fd, 0x5401, &mut [0u8; 8]), Err(LxError::ENOTTY));
}

#[test]
fn chattr_sets_and_lsattr_reads_the_flags() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let f = file_with(&root, "f", b"");
    let ctx = Ctx::root();
    let fd = ctx.open(f.clone(), OpenFlags::RDONLY);
    let mut flags = [0u8; 4];
    ctx.ioctl(fd, FS_IOC_GETFLAGS, &mut flags).unwrap();
    assert_eq!(u32::from_le_bytes(flags), 0);
    // chattr +C (no-COW), through the 32-bit number too.
    let nocow = 0x0080_0000u32;
    ctx.ioctl(fd, FS_IOC32_SETFLAGS, &mut nocow.to_le_bytes())
        .unwrap();
    ctx.ioctl(fd, FS_IOC_GETFLAGS, &mut flags).unwrap();
    assert_eq!(u32::from_le_bytes(flags), nocow);
    // chattr +i: the file can no longer be written.
    ctx.ioctl(
        fd,
        FS_IOC_SETFLAGS,
        &mut (nocow | FS_IMMUTABLE_FL).to_le_bytes(),
    )
    .unwrap();
    assert!(f.write_at(0, b"x").is_err());
}

#[test]
fn immutable_and_append_only_need_cap_linux_immutable_even_for_the_owner() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let f = file_with(&fs.root_inode(), "f", b"");
    let mut md = f.metadata().unwrap();
    md.uid = 1000;
    f.set_metadata(&md).unwrap();
    let user = Ctx::as_user(1000);
    let fd = user.open(f.clone(), OpenFlags::RDONLY);
    for flag in [FS_IMMUTABLE_FL, FS_APPEND_FL] {
        assert_eq!(
            user.ioctl(fd, FS_IOC_SETFLAGS, &mut flag.to_le_bytes()),
            Err(LxError::EPERM)
        );
    }
    // Other flags are the owner's to change; a stranger's are not.
    user.ioctl(fd, FS_IOC_SETFLAGS, &mut 0x0080_0000u32.to_le_bytes())
        .unwrap();
    let stranger = Ctx::as_user(1001);
    let fd = stranger.open(f, OpenFlags::RDONLY);
    assert_eq!(
        stranger.ioctl(fd, FS_IOC_SETFLAGS, &mut 0u32.to_le_bytes()),
        Err(LxError::EPERM)
    );
}

#[test]
fn ficlone_shares_the_blocks_instead_of_copying_them() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let data = pattern(1 << 20, 7);
    let src = file_with(&root, "src", &data);
    let dst = file_with(&root, "dst", b"");
    fs.sync().unwrap();
    let before = used(&fs);
    let ctx = Ctx::root();
    let s = ctx.open(src, OpenFlags::RDONLY);
    let d = ctx.open(dst.clone(), OpenFlags::RDWR);
    ctx.ioctl(d, FICLONE, &mut s.to_le_bytes()).unwrap();
    assert_eq!(read_all(&dst), data);
    fs.sync().unwrap();
    assert!(
        used(&fs) < before + (64 << 10),
        "1 MiB cloned, {} bytes more in use",
        used(&fs) - before
    );
}

#[test]
fn ficlone_wants_a_writable_destination_and_a_readable_source() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let src = file_with(&root, "src", b"abc");
    let dst = file_with(&root, "dst", b"");
    let ctx = Ctx::root();
    let s = ctx.open(src.clone(), OpenFlags::RDONLY);
    let d_ro = ctx.open(dst.clone(), OpenFlags::RDONLY);
    let d_append = ctx.open(dst.clone(), OpenFlags::RDWR | OpenFlags::APPEND);
    let s_wo = ctx.open(src, OpenFlags::WRONLY);
    let d = ctx.open(dst, OpenFlags::WRONLY);
    assert_eq!(
        ctx.ioctl(d_ro, FICLONE, &mut s.to_le_bytes()),
        Err(LxError::EBADF)
    );
    assert_eq!(
        ctx.ioctl(d_append, FICLONE, &mut s.to_le_bytes()),
        Err(LxError::EBADF)
    );
    assert_eq!(
        ctx.ioctl(d, FICLONE, &mut s_wo.to_le_bytes()),
        Err(LxError::EBADF)
    );
    assert_eq!(
        ctx.ioctl(d, FICLONE, &mut 99i32.to_le_bytes()),
        Err(LxError::EBADF)
    );
    // A directory cannot be cloned.
    let dir = ctx.open(root, OpenFlags::RDONLY);
    assert_eq!(
        ctx.ioctl(d, FICLONE, &mut dir.to_le_bytes()),
        Err(LxError::EISDIR)
    );
    // Nor a file onto itself.
    let both = ctx.open(src_named(&fs, "src"), OpenFlags::RDWR);
    assert_eq!(
        ctx.ioctl(both, FICLONE, &mut both.to_le_bytes()),
        Err(LxError::EINVAL)
    );
}

fn src_named(fs: &Arc<EclipseMountFs>, name: &str) -> Arc<dyn INode> {
    fs.root_inode().find(name).unwrap()
}

#[test]
fn ficlone_across_two_filesystems_is_exdev() {
    let a = mount(&formatted(16 << 20));
    let b = mount(&formatted(16 << 20));
    let ctx = Ctx::root();
    let s = ctx.open(file_with(&a.root_inode(), "s", b"x"), OpenFlags::RDONLY);
    let d = ctx.open(file_with(&b.root_inode(), "d", b""), OpenFlags::RDWR);
    assert_eq!(
        ctx.ioctl(d, FICLONE, &mut s.to_le_bytes()),
        Err(LxError::EXDEV)
    );
}

#[test]
fn ficlonerange_clones_an_aligned_range_into_the_middle() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let src_data = pattern(64 << 10, 1);
    let dst_data = pattern(64 << 10, 2);
    let ctx = Ctx::root();
    let s = ctx.open(file_with(&root, "s", &src_data), OpenFlags::RDONLY);
    let dst = file_with(&root, "d", &dst_data);
    let d = ctx.open(dst.clone(), OpenFlags::RDWR);
    // struct file_clone_range: src_fd, src_offset, src_length, dest_offset.
    let mut arg = [0u8; 32];
    arg[0..8].copy_from_slice(&(s as i64).to_le_bytes());
    arg[8..16].copy_from_slice(&8192u64.to_le_bytes());
    arg[16..24].copy_from_slice(&16384u64.to_le_bytes());
    arg[24..32].copy_from_slice(&4096u64.to_le_bytes());
    ctx.ioctl(d, FICLONERANGE, &mut arg).unwrap();
    let mut want = dst_data.clone();
    want[4096..20480].copy_from_slice(&src_data[8192..24576]);
    assert_eq!(read_all(&dst), want);
    // An offset off a block boundary is refused.
    arg[8..16].copy_from_slice(&100u64.to_le_bytes());
    assert_eq!(ctx.ioctl(d, FICLONERANGE, &mut arg), Err(LxError::EINVAL));
}

/// `struct file_dedupe_range` with one `file_dedupe_range_info` per fd.
fn dedupe_arg(src_off: u64, len: u64, dests: &[(i32, u64)]) -> Vec<u8> {
    let mut b = vec![0u8; 24 + 32 * dests.len()];
    b[0..8].copy_from_slice(&src_off.to_le_bytes());
    b[8..16].copy_from_slice(&len.to_le_bytes());
    b[16..18].copy_from_slice(&(dests.len() as u16).to_le_bytes());
    for (i, (fd, off)) in dests.iter().enumerate() {
        let at = 24 + 32 * i;
        b[at..at + 8].copy_from_slice(&(*fd as i64).to_le_bytes());
        b[at + 8..at + 16].copy_from_slice(&off.to_le_bytes());
    }
    b
}

fn dedupe_result(b: &[u8], i: usize) -> (u64, i32) {
    let at = 24 + 32 * i;
    (get64(b, at + 16), get32(b, at + 24) as i32)
}

#[test]
fn fideduperange_reports_same_differs_and_a_bad_fd_per_destination() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let data = pattern(256 << 10, 3);
    let ctx = Ctx::root();
    let s = ctx.open(file_with(&root, "s", &data), OpenFlags::RDONLY);
    let same = ctx.open(file_with(&root, "same", &data), OpenFlags::RDWR);
    let other = ctx.open(
        file_with(&root, "other", &pattern(256 << 10, 4)),
        OpenFlags::RDWR,
    );
    fs.sync().unwrap();
    let before = used(&fs);
    let mut arg = dedupe_arg(0, 256 << 10, &[(same, 0), (other, 0), (77, 0)]);
    ctx.ioctl(s, FIDEDUPERANGE, &mut arg).unwrap();
    assert_eq!(dedupe_result(&arg, 0), (256 << 10, FILE_DEDUPE_RANGE_SAME));
    assert_eq!(dedupe_result(&arg, 1), (0, FILE_DEDUPE_RANGE_DIFFERS));
    assert_eq!(dedupe_result(&arg, 2), (0, -(LxError::EBADF as i32)));
    fs.sync().unwrap();
    assert!(used(&fs) < before, "the duplicate's blocks were given back");
    assert_eq!(read_all(&src_named(&fs, "same")), data);
    // The reserved fields must be zero.
    let mut bad = dedupe_arg(0, 4096, &[(same, 0)]);
    bad[18] = 1;
    assert_eq!(ctx.ioctl(s, FIDEDUPERANGE, &mut bad), Err(LxError::EINVAL));
}

#[test]
fn subvolumes_and_snapshots_are_made_listed_and_deleted_by_ioctl() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let ctx = Ctx::root();
    let top = ctx.open(root.clone(), OpenFlags::RDONLY);
    ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "home"))
        .unwrap();
    let home = root.find("home").unwrap();
    file_with(&home, "notes", b"before");
    let h = ctx.open(home.clone(), OpenFlags::RDONLY);
    ctx.ioctl(
        top,
        BTRFS_IOC_SNAP_CREATE_V2,
        &mut vol_args_v2(h as i64, BTRFS_SUBVOL_RDONLY, "home-snap"),
    )
    .unwrap();
    // The directory listing sees both without a remount.
    let mut listed = names(&root);
    listed.sort();
    assert_eq!(listed, ["home", "home-snap"]);
    home.find("notes").unwrap().write_at(0, b"after!").unwrap();
    let snap = root.find("home-snap").unwrap();
    assert_eq!(read_all(&snap.find("notes").unwrap()), b"before");
    let sn = ctx.open(snap.clone(), OpenFlags::RDONLY);
    let mut flags = [0u8; 8];
    ctx.ioctl(sn, BTRFS_IOC_SUBVOL_GETFLAGS, &mut flags)
        .unwrap();
    assert_eq!(u64::from_le_bytes(flags), BTRFS_SUBVOL_RDONLY);
    assert!(snap.create("new", FileType::File, 0o644).is_err());
    ctx.ioctl(sn, BTRFS_IOC_SUBVOL_SETFLAGS, &mut 0u64.to_le_bytes())
        .unwrap();
    snap.create("new", FileType::File, 0o644).unwrap();
    ctx.ioctl(
        top,
        BTRFS_IOC_SNAP_DESTROY_V2,
        &mut vol_args_v2(0, 0, "home-snap"),
    )
    .unwrap();
    assert_eq!(names(&root), ["home"]);
    assert!(root.find("home-snap").is_err());
}

#[test]
fn subvolume_names_and_permissions_are_checked() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let ctx = Ctx::root();
    let top = ctx.open(root.clone(), OpenFlags::RDONLY);
    for bad in ["", ".", "..", "a/b"] {
        assert_eq!(
            ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, bad)),
            Err(LxError::EINVAL),
            "{:?}",
            bad
        );
    }
    let mut unterminated = vec![b'x'; 4096];
    unterminated[0..8].fill(0);
    assert_eq!(
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut unterminated),
        Err(LxError::ENAMETOOLONG)
    );
    // Creating needs write access to the directory; deleting, CAP_SYS_ADMIN.
    let user = Ctx::as_user(1000);
    let utop = user.open(root.clone(), OpenFlags::RDONLY);
    assert_eq!(
        user.ioctl(utop, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "mine")),
        Err(LxError::EACCES)
    );
    ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "sv"))
        .unwrap();
    assert_eq!(
        user.ioctl(utop, BTRFS_IOC_SNAP_DESTROY, &mut vol_args(0, "sv")),
        Err(LxError::EPERM)
    );
    // A file is not a directory to make a subvolume in.
    let f = ctx.open(file_with(&root, "f", b""), OpenFlags::RDONLY);
    assert_eq!(
        ctx.ioctl(f, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "x")),
        Err(LxError::ENOTDIR)
    );
    // Only a subvolume's root directory has subvolume flags.
    let d = root.create("plain", FileType::Dir, 0o755).unwrap();
    let d = ctx.open(d, OpenFlags::RDONLY);
    assert_eq!(
        ctx.ioctl(d, BTRFS_IOC_SUBVOL_GETFLAGS, &mut [0u8; 8]),
        Err(LxError::EINVAL)
    );
}

#[test]
fn get_subvol_info_and_ino_lookup_describe_where_a_file_is() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let ctx = Ctx::root();
    let top = ctx.open(root.clone(), OpenFlags::RDONLY);
    ctx.ioctl(
        top,
        BTRFS_IOC_SUBVOL_CREATE_V2,
        &mut vol_args_v2(0, 0, "sv"),
    )
    .unwrap();
    let sv = root.find("sv").unwrap();
    let deep = sv
        .create("a", FileType::Dir, 0o755)
        .unwrap()
        .create("b", FileType::Dir, 0o755)
        .unwrap();
    let fd = ctx.open(sv.clone(), OpenFlags::RDONLY);
    let mut info = vec![0u8; 504];
    ctx.ioctl(fd, BTRFS_IOC_GET_SUBVOL_INFO, &mut info).unwrap();
    let id = get64(&info, 0);
    assert!(id > FS_TREE);
    assert_eq!(cstr(&info, 8, 256).unwrap(), "sv");
    assert_eq!(get64(&info, 264), FS_TREE, "its parent is the top");
    assert_eq!(get64(&info, 272), FIRST_INO, "named in the top's root dir");
    assert_eq!(get64(&info, 288), 0, "writable");
    // INO_LOOKUP: the path of `a/b` inside the subvolume.
    let mut arg = vec![0u8; 4096];
    arg[8..16].copy_from_slice(&split(deep.metadata().unwrap().inode as u64).1.to_le_bytes());
    ctx.ioctl(fd, BTRFS_IOC_INO_LOOKUP, &mut arg).unwrap();
    assert_eq!(get64(&arg, 0), id);
    assert_eq!(cstr(&arg, 16, 4080).unwrap(), "a/b/");
    // Its own root, which any user may ask about.
    let user = Ctx::as_user(1000);
    let ufd = user.open(sv, OpenFlags::RDONLY);
    let mut arg = vec![0u8; 4096];
    arg[8..16].copy_from_slice(&FIRST_INO.to_le_bytes());
    user.ioctl(ufd, BTRFS_IOC_INO_LOOKUP, &mut arg).unwrap();
    assert_eq!((get64(&arg, 0), cstr(&arg, 16, 4080).unwrap()), (id, ""));
}

#[test]
fn the_default_subvolume_is_what_the_next_mount_shows() {
    let disk = formatted(16 << 20);
    {
        let fs = mount(&disk);
        let root = fs.root_inode();
        let ctx = Ctx::root();
        let top = ctx.open(root.clone(), OpenFlags::RDONLY);
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "rootfs"))
            .unwrap();
        let sv = root.find("rootfs").unwrap();
        file_with(&sv, "etc-hostname", b"eclipse");
        let fd = ctx.open(sv, OpenFlags::RDONLY);
        let mut info = vec![0u8; 504];
        ctx.ioctl(fd, BTRFS_IOC_GET_SUBVOL_INFO, &mut info).unwrap();
        let user = Ctx::as_user(5);
        let utop = user.open(root.clone(), OpenFlags::RDONLY);
        assert_eq!(
            user.ioctl(
                utop,
                BTRFS_IOC_DEFAULT_SUBVOL,
                &mut get64(&info, 0).to_le_bytes()
            ),
            Err(LxError::EPERM)
        );
        ctx.ioctl(
            top,
            BTRFS_IOC_DEFAULT_SUBVOL,
            &mut get64(&info, 0).to_le_bytes(),
        )
        .unwrap();
        ctx.ioctl(top, BTRFS_IOC_SYNC, &mut []).unwrap();
    }
    let fs = mount(&disk);
    assert_eq!(
        read_all(&fs.root_inode().find("etc-hostname").unwrap()),
        b"eclipse"
    );
    // subvolid=5 still reaches the top.
    let top = mount_with(&disk, false, "subvolid=5");
    assert!(top.root_inode().find("rootfs").is_ok());
}

#[test]
fn fs_info_and_dev_info_describe_a_single_device() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let ctx = Ctx::root();
    let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
    let mut info = vec![0u8; 1024];
    put64(
        &mut info,
        48,
        BTRFS_FS_INFO_FLAG_GENERATION | BTRFS_FS_INFO_FLAG_CSUM_INFO,
    );
    ctx.ioctl(fd, BTRFS_IOC_FS_INFO, &mut info).unwrap();
    assert_eq!((get64(&info, 0), get64(&info, 8)), (1, 1));
    assert_eq!(&info[16..32], &[9u8; 16]);
    assert_eq!(get32(&info, 32), 4096);
    assert_eq!(get16(&info, 46), 4, "CRC32C");
    assert_eq!(
        get64(&info, 48),
        BTRFS_FS_INFO_FLAG_GENERATION | BTRFS_FS_INFO_FLAG_CSUM_INFO
    );
    assert_eq!(get64(&info, 56), fs.inner.lock().generation());
    let mut dev = vec![0u8; 4096];
    put64(&mut dev, 0, 1);
    ctx.ioctl(fd, BTRFS_IOC_DEV_INFO, &mut dev).unwrap();
    assert_eq!(get64(&dev, 32), 16 << 20);
    assert_eq!(&dev[40..56], &[9u8; 16]);
    put64(&mut dev, 0, 2);
    assert_eq!(
        ctx.ioctl(fd, BTRFS_IOC_DEV_INFO, &mut dev),
        Err(LxError::ENODEV)
    );
    // START_SYNC hands back the generation it committed.
    file_with(&fs.root_inode(), "x", b"x");
    let mut gen = [0u8; 8];
    ctx.ioctl(fd, BTRFS_IOC_START_SYNC, &mut gen).unwrap();
    assert_eq!(u64::from_le_bytes(gen), fs.inner.lock().generation());
}

#[test]
fn scrub_reads_everything_on_the_first_device_and_needs_cap_sys_admin() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    file_with(&fs.root_inode(), "data", &pattern(200 << 10, 9));
    let ctx = Ctx::root();
    let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
    let mut arg = vec![0u8; 1024];
    put64(&mut arg, 0, 1);
    ctx.ioctl(fd, BTRFS_IOC_SCRUB, &mut arg).unwrap();
    assert!(get64(&arg, 32) >= 50, "data blocks: {}", get64(&arg, 32));
    assert!(get64(&arg, 40) > 0, "tree nodes");
    assert_eq!(get64(&arg, 72) + get64(&arg, 80) + get64(&arg, 120), 0);
    put64(&mut arg, 0, 3);
    assert_eq!(
        ctx.ioctl(fd, BTRFS_IOC_SCRUB, &mut arg),
        Err(LxError::ENODEV)
    );
    let user = Ctx::as_user(1000);
    let ufd = user.open(fs.root_inode(), OpenFlags::RDONLY);
    put64(&mut arg, 0, 1);
    assert_eq!(
        user.ioctl(ufd, BTRFS_IOC_SCRUB, &mut arg),
        Err(LxError::EPERM)
    );
    assert_eq!(
        ctx.ioctl(fd, BTRFS_IOC_SCRUB_PROGRESS, &mut arg),
        Err(LxError::ENOTCONN)
    );
}

#[test]
fn a_qgroup_limit_set_by_ioctl_stops_writes_past_it() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let ctx = Ctx::root();
    let top = ctx.open(root.clone(), OpenFlags::RDONLY);
    ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "q"))
        .unwrap();
    let q = root.find("q").unwrap();
    let qfd = ctx.open(q.clone(), OpenFlags::RDONLY);
    let mut ctl = [0u8; 16];
    put64(&mut ctl, 0, 1);
    ctx.ioctl(top, BTRFS_IOC_QUOTA_CTL, &mut ctl).unwrap();
    // qgroupid 0: the subvolume the fd is in.
    let mut lim = [0u8; 48];
    put64(&mut lim, 8, BTRFS_QGROUP_LIMIT_MAX_RFER);
    put64(&mut lim, 16, 256 << 10);
    ctx.ioctl(qfd, BTRFS_IOC_QGROUP_LIMIT, &mut lim).unwrap();
    let f = file_with(&q, "big", b"");
    let chunk = pattern(64 << 10, 5);
    let mut written = 0;
    let refused = loop {
        match f.write_at(written, &chunk) {
            Ok(n) if n > 0 && written < (1 << 20) => written += n,
            Ok(_) => break None,
            Err(e) => break Some(e),
        }
    };
    assert!(
        matches!(refused, Some(FsError::NoDeviceSpace)),
        "{:?}",
        refused
    );
    assert!(written <= 256 << 10);
    // `limit none`.
    put64(&mut lim, 16, u64::MAX);
    ctx.ioctl(qfd, BTRFS_IOC_QGROUP_LIMIT, &mut lim).unwrap();
    f.write_at(written, &chunk).unwrap();
    // Higher-level qgroups and exclusive limits are not kept.
    put64(&mut lim, 0, 1 << 48 | 5);
    assert_eq!(
        ctx.ioctl(qfd, BTRFS_IOC_QGROUP_LIMIT, &mut lim),
        Err(LxError::EOPNOTSUPP)
    );
    put64(&mut ctl, 0, 9);
    assert_eq!(
        ctx.ioctl(top, BTRFS_IOC_QUOTA_CTL, &mut ctl),
        Err(LxError::EINVAL)
    );
    put64(&mut ctl, 0, 2);
    ctx.ioctl(top, BTRFS_IOC_QUOTA_CTL, &mut ctl).unwrap();
    assert!(!fs.inner.lock().quota_enabled());
}

#[test]
fn a_read_only_mount_refuses_every_change_but_answers_questions() {
    let disk = formatted(16 << 20);
    {
        let fs = mount(&disk);
        file_with(&fs.root_inode(), "f", b"x");
        fs.sync().unwrap();
    }
    let fs = mount_with(&disk, true, "");
    let root = fs.root_inode();
    let ctx = Ctx::root();
    let top = ctx.open(root.clone(), OpenFlags::RDONLY);
    let f = ctx.open(root.find("f").unwrap(), OpenFlags::RDONLY);
    let mut flags = [0u8; 4];
    ctx.ioctl(f, FS_IOC_GETFLAGS, &mut flags).unwrap();
    assert_eq!(
        ctx.ioctl(f, FS_IOC_SETFLAGS, &mut 0u32.to_le_bytes()),
        Err(LxError::EROFS)
    );
    assert_eq!(
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "x")),
        Err(LxError::EROFS)
    );
    let mut ctl = [0u8; 16];
    put64(&mut ctl, 0, 1);
    assert_eq!(
        ctx.ioctl(top, BTRFS_IOC_QUOTA_CTL, &mut ctl),
        Err(LxError::EROFS)
    );
    let inode = root.find("f").unwrap();
    assert_eq!(
        eclipsefs_xattr(
            &inode,
            "/efs-test/f",
            XattrRequest::Set {
                name: "user.a",
                value: b"1",
                flags: 0
            },
            0,
            true
        ),
        Some(Err(LxError::EROFS))
    );
}

fn xattr_as(inode: &Arc<dyn INode>, req: XattrRequest<'_>, euid: u32) -> LxResult<XattrReply> {
    eclipsefs_xattr(inode, "/efs-test/f", req, euid, euid == 0).unwrap()
}

#[test]
fn extended_attributes_round_trip_through_the_syscall_layer() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let f = file_with(&fs.root_inode(), "f", b"");
    let set = |name, value: &'static [u8], flags| XattrRequest::Set { name, value, flags };
    assert_eq!(
        xattr_as(&f, set("user.mime", b"text/plain", 0), 0),
        Ok(XattrReply::Done)
    );
    assert_eq!(
        xattr_as(&f, XattrRequest::Get("user.mime"), 0),
        Ok(XattrReply::Value(b"text/plain".to_vec()))
    );
    assert_eq!(
        xattr_as(&f, set("trusted.t", b"1", 0), 0),
        Ok(XattrReply::Done)
    );
    assert_eq!(
        xattr_as(&f, XattrRequest::List, 0),
        Ok(XattrReply::Names(b"trusted.t\0user.mime\0".to_vec()))
    );
    // A user sees no trusted.* name and may not read one.
    assert_eq!(
        xattr_as(&f, XattrRequest::List, 1000),
        Ok(XattrReply::Names(b"user.mime\0".to_vec()))
    );
    assert_eq!(
        xattr_as(&f, XattrRequest::Get("trusted.t"), 1000),
        Err(LxError::EPERM)
    );
    // Nor change another's attributes.
    assert_eq!(
        xattr_as(&f, set("user.x", b"1", 0), 1000),
        Err(LxError::EPERM)
    );
    // XATTR_CREATE on an existing one, XATTR_REPLACE on a missing one.
    assert_eq!(
        xattr_as(&f, set("user.mime", b"x", 1), 0),
        Err(LxError::EEXIST)
    );
    assert_eq!(
        xattr_as(&f, set("user.none", b"x", 2), 0),
        Err(LxError::ENODATA)
    );
    assert_eq!(
        xattr_as(&f, XattrRequest::Get("user.none"), 0),
        Err(LxError::ENODATA)
    );
    assert_eq!(
        xattr_as(&f, XattrRequest::Remove("user.mime"), 0),
        Ok(XattrReply::Done)
    );
    assert_eq!(
        xattr_as(&f, XattrRequest::Remove("user.mime"), 0),
        Err(LxError::ENODATA)
    );
    // user.* only on files and directories.
    let link = fs
        .root_inode()
        .create("l", FileType::SymLink, 0o777)
        .unwrap();
    link.write_at(0, b"f").unwrap();
    assert_eq!(
        xattr_as(&link, set("user.x", b"1", 0), 0),
        Err(LxError::EPERM)
    );
}

#[test]
fn the_btrfs_compression_property_is_the_files_compression() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let f = file_with(&fs.root_inode(), "f", b"");
    assert_eq!(
        xattr_as(&f, XattrRequest::Get(COMPRESSION_XATTR), 0),
        Err(LxError::ENODATA)
    );
    let set = |value: &'static [u8]| XattrRequest::Set {
        name: COMPRESSION_XATTR,
        value,
        flags: 0,
    };
    xattr_as(&f, set(b"zstd"), 0).unwrap();
    assert_eq!(
        xattr_as(&f, XattrRequest::Get(COMPRESSION_XATTR), 0),
        Ok(XattrReply::Value(b"zstd".to_vec()))
    );
    assert_eq!(xattr_as(&f, set(b"brotli"), 0), Err(LxError::EINVAL));
    xattr_as(&f, set(b""), 0).unwrap();
    assert_eq!(
        xattr_as(&f, XattrRequest::Get(COMPRESSION_XATTR), 0),
        Err(LxError::ENODATA)
    );
}

#[test]
fn a_punched_hole_reads_as_zeros_and_gives_its_blocks_back() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let data = pattern(1 << 20, 11);
    let inode = file_with(&fs.root_inode(), "f", &data);
    fs.sync().unwrap();
    let before = used(&fs);
    let file = File::new(inode.clone(), OpenFlags::RDWR, String::from("/efs-test/f"));
    // FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE.
    eclipsefs_fallocate(&file, 0x3, 4096, 512 << 10)
        .unwrap()
        .unwrap();
    let mut want = data;
    want[4096..4096 + (512 << 10)].fill(0);
    assert_eq!(read_all(&inode), want);
    fs.sync().unwrap();
    assert!(used(&fs) + (400 << 10) < before);
    // Plain preallocation grows the file; KEEP_SIZE does not.
    eclipsefs_fallocate(&file, 0, 1 << 20, 1 << 20)
        .unwrap()
        .unwrap();
    assert_eq!(inode.metadata().unwrap().size, 2 << 20);
    eclipsefs_fallocate(&file, 0x1, 2 << 20, 1 << 20)
        .unwrap()
        .unwrap();
    assert_eq!(inode.metadata().unwrap().size, 2 << 20);
}

/// `struct fiemap` asking for `count` extents of `start..start+len`.
fn fiemap_args(start: u64, len: u64, flags: u32, count: u32) -> Vec<u8> {
    let mut b = vec![0u8; FIEMAP_HEADER + FIEMAP_EXTENT * count as usize];
    put64(&mut b, 0, start);
    put64(&mut b, 8, len);
    put32(&mut b, 16, flags);
    put32(&mut b, 24, count);
    b
}

/// The extents a FIEMAP answer holds, as (logical, length, flags).
fn fiemap_extents(b: &[u8]) -> Vec<(u64, u64, u32)> {
    (0..get32(b, 20) as usize)
        .map(|i| {
            let e = &b[FIEMAP_HEADER + i * FIEMAP_EXTENT..];
            (get64(e, 0), get64(e, 16), get32(e, 40))
        })
        .collect()
}

/// data [0,256K) | hole | data [512K,768K), size 1 MiB.
fn sparse_file(fs: &Arc<EclipseMountFs>) -> Arc<dyn INode> {
    let f = file_with(&fs.root_inode(), "sparse", &pattern(256 << 10, 1));
    assert_eq!(
        f.write_at(512 << 10, &pattern(256 << 10, 2)).unwrap(),
        256 << 10
    );
    f.resize(1 << 20).unwrap();
    fs.sync().unwrap();
    f
}

/// Two files written a block at a time, in turns and in separate
/// transactions, so each one's blocks alternate with the other's.
fn fragmented_pair(fs: &Arc<EclipseMountFs>, blocks: usize) -> [Arc<dyn INode>; 2] {
    let root = fs.root_inode();
    let a = file_with(&root, "a", b"");
    let b = file_with(&root, "b", b"");
    for i in 0..blocks {
        let at = i * BLOCK as usize;
        a.write_at(at, &pattern(BLOCK as usize, i as u8)).unwrap();
        b.write_at(at, &pattern(BLOCK as usize, i as u8 ^ 0x80))
            .unwrap();
        fs.sync().unwrap();
    }
    [a, b]
}

fn defrag_range_args(start: u64, len: u64, flags: u64, thresh: u32, algo: u32) -> Vec<u8> {
    let mut b = vec![0u8; 48];
    put64(&mut b, 0, start);
    put64(&mut b, 8, len);
    put64(&mut b, 16, flags);
    put32(&mut b, 24, thresh);
    put32(&mut b, 28, algo);
    b
}

fn extent_count(ctx: &Ctx, fd: i32) -> usize {
    let mut b = fiemap_args(0, u64::MAX, 0, 64);
    ctx.ioctl(fd, FS_IOC_FIEMAP, &mut b).unwrap();
    fiemap_extents(&b).len()
}

#[test]
fn defrag_makes_a_fragmented_file_one_extent() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let [a, _] = fragmented_pair(&fs, 16);
    let data = read_all(&a);
    let ctx = Ctx::root();
    let fd = ctx.open(a.clone(), OpenFlags::RDWR);
    assert_eq!(extent_count(&ctx, fd), 16);
    ctx.ioctl(fd, BTRFS_IOC_DEFRAG, &mut []).unwrap();
    assert_eq!(extent_count(&ctx, fd), 1);
    assert_eq!(read_all(&a), data);
    fs.inner.lock().check().unwrap();
}

#[test]
fn defrag_range_compresses_and_honours_the_range() {
    const ENCODED: u32 = 0x8;
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let [a, b] = fragmented_pair(&fs, 16);
    let ctx = Ctx::root();
    let fa = ctx.open(a.clone(), OpenFlags::RDWR);
    // Blocks 4..8 only: three runs remain around the new one.
    let mut r = defrag_range_args(4 * BLOCK, 4 * BLOCK, 0, 0, 0);
    ctx.ioctl(fa, BTRFS_IOC_DEFRAG_RANGE, &mut r).unwrap();
    assert_eq!(extent_count(&ctx, fa), 4 + 1 + 8);
    // `-c zstd` over the whole file (len 0 runs to the end).
    let fb = ctx.open(b.clone(), OpenFlags::RDWR);
    let data = read_all(&b);
    let mut r = defrag_range_args(
        0,
        0,
        BTRFS_DEFRAG_RANGE_COMPRESS | BTRFS_DEFRAG_RANGE_START_IO,
        0,
        3,
    );
    ctx.ioctl(fb, BTRFS_IOC_DEFRAG_RANGE, &mut r).unwrap();
    let mut m = fiemap_args(0, u64::MAX, 0, 64);
    ctx.ioctl(fb, FS_IOC_FIEMAP, &mut m).unwrap();
    assert!(fiemap_extents(&m).iter().all(|e| e.2 & ENCODED != 0));
    assert_eq!(read_all(&b), data);
    fs.inner.lock().check().unwrap();
}

#[test]
fn defrag_refuses_bad_arguments_and_strangers() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let [a, _] = fragmented_pair(&fs, 2);
    let ctx = Ctx::root();
    let fd = ctx.open(a.clone(), OpenFlags::RDWR);
    let mut r = defrag_range_args(0, 0, 1 << 5, 0, 0);
    assert_eq!(
        ctx.ioctl(fd, BTRFS_IOC_DEFRAG_RANGE, &mut r),
        Err(LxError::EOPNOTSUPP)
    );
    let mut r = defrag_range_args(0, 0, BTRFS_DEFRAG_RANGE_COMPRESS, 0, 9);
    assert_eq!(
        ctx.ioctl(fd, BTRFS_IOC_DEFRAG_RANGE, &mut r),
        Err(LxError::EINVAL)
    );
    // Someone else, with the file open only for reading.
    let user = Ctx::as_user(1000);
    let ro = user.open(a.clone(), OpenFlags::RDONLY);
    assert_eq!(
        user.ioctl(ro, BTRFS_IOC_DEFRAG, &mut []),
        Err(LxError::EPERM)
    );
    // A directory: CAP_SYS_ADMIN, and nothing to do.
    let dfd = user.open(fs.root_inode(), OpenFlags::RDONLY);
    assert_eq!(
        user.ioctl(dfd, BTRFS_IOC_DEFRAG, &mut []),
        Err(LxError::EPERM)
    );
    let dfd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
    ctx.ioctl(dfd, BTRFS_IOC_DEFRAG, &mut []).unwrap();
}

#[test]
fn defrag_writes_back_the_page_cache_first() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let [a, _] = fragmented_pair(&fs, 8);
    let ctx = Ctx::root();
    let fd = ctx.open(a.clone(), OpenFlags::RDWR);
    // A write through the descriptor that may still sit in the cache.
    let file = ctx.fd(fd);
    file.write_at(BLOCK, &[0xee; 100]).unwrap();
    let want = {
        let mut v = read_all(&a);
        v[BLOCK as usize..BLOCK as usize + 100].fill(0xee);
        v
    };
    ctx.ioctl(fd, BTRFS_IOC_DEFRAG, &mut []).unwrap();
    assert_eq!(read_all(&a), want);
    assert_eq!(extent_count(&ctx, fd), 1);
}

#[test]
fn memparse_reads_sizes_as_btrfs_progs_writes_them() {
    assert_eq!(memparse("4096"), Some(4096));
    assert_eq!(memparse("8m"), Some(8 << 20));
    assert_eq!(memparse("2G"), Some(2 << 30));
    assert_eq!(memparse("1e"), Some(1 << 60));
    assert_eq!(memparse("0"), None);
    assert_eq!(memparse(""), None);
    assert_eq!(memparse("12x"), None);
    assert_eq!(memparse("m"), None);
    assert_eq!(memparse("99999999999e"), None);
}

#[test]
fn resize_shrinks_grows_and_reads_relative_sizes() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let data = pattern(3 << 20, 4);
    let f = file_with(&fs.root_inode(), "f", &data);
    let ctx = Ctx::root();
    let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
    let size = || fs.inner.lock().fsinfo().total_bytes;
    ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "-8m"))
        .unwrap();
    assert_eq!(size(), 24 << 20);
    ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "1:+4m"))
        .unwrap();
    assert_eq!(size(), 28 << 20);
    ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "16m"))
        .unwrap();
    assert_eq!(size(), 16 << 20);
    ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "max"))
        .unwrap();
    assert_eq!(size(), 32 << 20);
    assert_eq!(read_all(&f), data);
    fs.inner.lock().check().unwrap();
}

#[test]
fn resize_refuses_bad_sizes_devices_and_users() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let ctx = Ctx::root();
    let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
    for bad in ["", "big", "1:", "x:8m", "64m", "-64m"] {
        assert_eq!(
            ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, bad)),
            Err(LxError::EINVAL),
            "{bad}"
        );
    }
    assert_eq!(
        ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "7:8m")),
        Err(LxError::ENODEV)
    );
    assert_eq!(
        ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "7:max")),
        Err(LxError::ENODEV)
    );
    let user = Ctx::as_user(1000);
    let ufd = user.open(fs.root_inode(), OpenFlags::RDONLY);
    assert_eq!(
        user.ioctl(ufd, BTRFS_IOC_RESIZE, &mut vol_args(0, "8m")),
        Err(LxError::EPERM)
    );
    // Too full to shrink that far.
    file_with(&fs.root_inode(), "big", &pattern(12 << 20, 1));
    fs.sync().unwrap();
    assert_eq!(
        ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "8m")),
        Err(LxError::ENOSPC)
    );
    assert_eq!(fs.inner.lock().fsinfo().total_bytes, 16 << 20);
}

#[test]
fn fiemap_maps_the_data_and_counts_with_no_room() {
    const LAST: u32 = 0x1;
    const SHARED: u32 = 0x2000;
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let ctx = Ctx::root();
    let f = sparse_file(&fs);
    let fd = ctx.open(f.clone(), OpenFlags::RDONLY);
    let mut b = fiemap_args(0, u64::MAX, 0, 16);
    ctx.ioctl(fd, FS_IOC_FIEMAP, &mut b).unwrap();
    let ext = fiemap_extents(&b);
    let spans: Vec<_> = ext
        .iter()
        .map(|&(l, n, _)| (l >> 10, (l + n) >> 10))
        .collect();
    // Writes may land in more than one extent; together they cover the
    // two data ranges exactly.
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (s, t) in spans {
        match merged.last_mut() {
            Some(m) if m.1 == s => m.1 = t,
            _ => merged.push((s, t)),
        }
    }
    assert_eq!(merged, [(0, 256), (512, 768)]);
    assert_eq!(ext.last().unwrap().2 & LAST, LAST);
    assert!(ext.iter().all(|e| e.2 & SHARED == 0));
    // No room: only the count comes back.
    let mut c = fiemap_args(0, u64::MAX, 0, 0);
    ctx.ioctl(fd, FS_IOC_FIEMAP, &mut c).unwrap();
    assert_eq!(get32(&c, 20) as usize, ext.len());
    // Room for one: the first extent, without LAST unless it is.
    let mut one = fiemap_args(0, u64::MAX, 0, 1);
    ctx.ioctl(fd, FS_IOC_FIEMAP, &mut one).unwrap();
    assert_eq!(fiemap_extents(&one), ext[..1]);
    // A reflinked copy shares its blocks.
    let g = file_with(&fs.root_inode(), "copy", b"");
    let gfd = ctx.open(g, OpenFlags::RDWR);
    let mut arg = (fd as i32).to_le_bytes().to_vec();
    ctx.ioctl(gfd, FICLONE, &mut arg).unwrap();
    let mut b = fiemap_args(0, u64::MAX, 1, 16);
    ctx.ioctl(gfd, FS_IOC_FIEMAP, &mut b).unwrap();
    assert!(fiemap_extents(&b).iter().all(|e| e.2 & SHARED != 0));
}

#[test]
fn fiemap_refuses_unknown_flags_and_an_empty_range() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let ctx = Ctx::root();
    let fd = ctx.open(file_with(&fs.root_inode(), "f", b"x"), OpenFlags::RDONLY);
    // FIEMAP_FLAG_XATTR (2) is not supported: EBADR, and fm_flags says
    // which flags were not understood.
    let mut b = fiemap_args(0, u64::MAX, 0x3, 4);
    assert_eq!(ctx.ioctl(fd, FS_IOC_FIEMAP, &mut b), Err(LxError::EBADR));
    assert_eq!(get32(&b, 16), 0x2);
    let mut b = fiemap_args(0, 0, 0, 4);
    assert_eq!(ctx.ioctl(fd, FS_IOC_FIEMAP, &mut b), Err(LxError::EINVAL));
    // Inline data: one piece, flagged as such.
    let mut b = fiemap_args(0, u64::MAX, 0, 4);
    ctx.ioctl(fd, FS_IOC_FIEMAP, &mut b).unwrap();
    assert_eq!(fiemap_extents(&b), [(0, 1, 0x1 | 0x100 | 0x200)]);
    // A directory maps nothing.
    let dfd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
    let mut b = fiemap_args(0, u64::MAX, 0, 4);
    ctx.ioctl(dfd, FS_IOC_FIEMAP, &mut b).unwrap();
    assert_eq!(get32(&b, 20), 0);
}

#[test]
fn seek_data_and_seek_hole_follow_the_extents() {
    const DATA: usize = 3;
    const HOLE: usize = 4;
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let f = sparse_file(&fs);
    let file = File::new(f.clone(), OpenFlags::RDONLY, String::from("/efs-test/f"));
    let seek = |w, off: i64| eclipsefs_seek(&file, w, off).unwrap();
    assert_eq!(seek(DATA, 0), Ok(0));
    assert_eq!(seek(HOLE, 0), Ok(256 << 10));
    assert_eq!(seek(DATA, 300 << 10), Ok(512 << 10));
    assert_eq!(seek(HOLE, 600 << 10), Ok(768 << 10));
    // Only a hole after the last data, and nothing at or past the end.
    assert_eq!(seek(DATA, 800 << 10), Err(LxError::ENXIO));
    assert_eq!(seek(HOLE, 800 << 10), Ok(800 << 10));
    assert_eq!(seek(DATA, 1 << 20), Err(LxError::ENXIO));
    assert_eq!(seek(HOLE, -1), Err(LxError::ENXIO));
    // Data written through the page cache counts before it is synced.
    assert_eq!(f.write_at(300 << 10, b"late").unwrap(), 4);
    assert_eq!(seek(DATA, 260 << 10), Ok(300 << 10 & !4095));
}

#[test]
fn atomic_writes_go_through_whole_and_say_their_unit() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let inode = file_with(&root, "a", &pattern(300_000, 3));
    assert_eq!(
        eclipsefs_atomic_write_unit(&inode),
        Some((4096, eclipsefs::cow::ATOMIC_WRITE_MAX as u32))
    );
    let file = File::new(inode.clone(), OpenFlags::RDWR, String::from("/efs-test/a"));
    let data = pattern(65_536, 9);
    assert_eq!(file.write_atomic_at(65_536, &data), Ok(65_536));
    let mut back = vec![0u8; 65_536];
    assert_eq!(inode.read_at(65_536, &mut back).unwrap(), 65_536);
    assert_eq!(back, data);
    // Read-only descriptors and directories.
    let ro = File::new(
        inode.clone(),
        OpenFlags::RDONLY,
        String::from("/efs-test/a"),
    );
    assert_eq!(ro.write_atomic_at(0, &data), Err(LxError::EBADF));
    let dir = root.create("d", FileType::Dir, 0o755).unwrap();
    assert_eq!(eclipsefs_atomic_write_unit(&dir), None);
    // Too big for one atomic write.
    let big = vec![0u8; eclipsefs::cow::ATOMIC_WRITE_MAX as usize * 2];
    assert_eq!(file.write_atomic_at(0, &big), Err(LxError::EINVAL));
}

#[test]
fn copy_file_range_reflinks_aligned_ranges_and_leaves_the_rest_to_a_copy() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let data = pattern((1 << 20) + 123, 13);
    let src = File::new(
        file_with(&root, "s", &data),
        OpenFlags::RDONLY,
        String::from("/efs-test/s"),
    );
    let dst_inode = file_with(&root, "d", b"");
    let dst = File::new(
        dst_inode.clone(),
        OpenFlags::RDWR,
        String::from("/efs-test/d"),
    );
    // Unaligned: not ours to do.
    assert!(eclipsefs_copy_reflink(&src, 1, &dst, 0, 4096).is_none());
    // Aligned and up to the end of the source, odd tail included.
    assert_eq!(
        eclipsefs_copy_reflink(&src, 0, &dst, 0, u64::MAX),
        Some(Ok(data.len() as u64))
    );
    assert_eq!(read_all(&dst_inode), data);
    // A length that is not whole blocks short of the end is cut to blocks.
    let dst2 = File::new(
        file_with(&root, "d2", b""),
        OpenFlags::RDWR,
        String::from("/efs-test/d2"),
    );
    assert_eq!(
        eclipsefs_copy_reflink(&src, 0, &dst2, 0, 10_000),
        Some(Ok(8192))
    );
    assert!(eclipsefs_copy_reflink(&src, 0, &dst2, 0, 100).is_none());
    // Different filesystems copy.
    let other = mount(&formatted(16 << 20));
    let far = File::new(
        file_with(&other.root_inode(), "x", b""),
        OpenFlags::RDWR,
        String::from("/efs-test/x"),
    );
    assert!(eclipsefs_copy_reflink(&src, 0, &far, 0, 4096).is_none());
}

#[test]
fn a_snapshot_sent_into_a_file_is_received_back_into_a_directory() {
    let disk = formatted(48 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let ctx = Ctx::root();
    let top = ctx.open(root.clone(), OpenFlags::RDONLY);
    ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "src"))
        .unwrap();
    let src = root.find("src").unwrap();
    let body = pattern(300 << 10, 17);
    file_with(&src, "file", &body);
    src.create("dir", FileType::Dir, 0o755).unwrap();
    let s = ctx.open(src, OpenFlags::RDONLY);
    ctx.ioctl(
        top,
        BTRFS_IOC_SNAP_CREATE_V2,
        &mut vol_args_v2(s as i64, BTRFS_SUBVOL_RDONLY, "snap"),
    )
    .unwrap();
    let snap = ctx.open(root.find("snap").unwrap(), OpenFlags::RDONLY);
    let stream = ctx.open(file_with(&root, "stream", b""), OpenFlags::RDWR);
    // struct btrfs_ioctl_send_args: send_fd, clone count/sources, parent, flags.
    let mut send = [0u8; 72];
    send[0..8].copy_from_slice(&(stream as i64).to_le_bytes());
    ctx.ioctl(snap, BTRFS_IOC_SEND, &mut send).unwrap();
    let len = ctx.fd(stream).metadata().unwrap().size;
    assert!(
        len > body.len(),
        "the stream carries the data: {} bytes",
        len
    );
    // Receive it from the start of the stream file.
    let back = root.create("back", FileType::Dir, 0o755).unwrap();
    let b = ctx.open(back.clone(), OpenFlags::RDONLY);
    let input = ctx.open(src_named(&fs, "stream"), OpenFlags::RDONLY);
    let mut recv = [0u8; 32];
    recv[0..8].copy_from_slice(&(input as i64).to_le_bytes());
    ctx.ioctl(b, EFS_IOC_RECEIVE, &mut recv).unwrap();
    assert_eq!(
        ctx.fd(input).seek(SeekFrom::Current(0)).unwrap(),
        len as u64,
        "the stream was read to its end"
    );
    let got = names(&back);
    assert_eq!(got.len(), 1, "{:?}", got);
    let copy = back.find(&got[0]).unwrap();
    assert_eq!(read_all(&copy.find("file").unwrap()), body);
    assert!(copy.find("dir").is_ok());
    // Sending needs CAP_SYS_ADMIN, and a file, not a directory, to go to.
    let user = Ctx::as_user(1000);
    let usnap = user.open(root.find("snap").unwrap(), OpenFlags::RDONLY);
    assert_eq!(
        user.ioctl(usnap, BTRFS_IOC_SEND, &mut send),
        Err(LxError::EPERM)
    );
    let mut to_dir = [0u8; 72];
    to_dir[0..8].copy_from_slice(&(top as i64).to_le_bytes());
    assert!(ctx.ioctl(snap, BTRFS_IOC_SEND, &mut to_dir).is_err());
}

#[test]
fn mount_options_are_read_and_bad_values_refused() {
    let o = EfsOptions::parse("rw,noatime,compress=zstd:3,degraded,device=/dev/sdb2,subvol=/@home")
        .unwrap();
    assert_eq!(o.compress, Some(Some(Compression::Zstd)));
    assert!(o.degraded);
    assert_eq!(o.devices, ["sdb2"]);
    assert_eq!(o.subvol.as_deref(), Some("/@home"));
    assert_eq!(
        EfsOptions::parse("compress=no").unwrap().compress,
        Some(None)
    );
    assert_eq!(
        EfsOptions::parse("compress").unwrap().compress,
        Some(Some(Compression::Zlib))
    );
    assert_eq!(
        EfsOptions::parse("subvolid=257").unwrap().subvolid,
        Some(257)
    );
    assert_eq!(EfsOptions::parse("commit=30").unwrap().commit, Some(30));
    assert!(EfsOptions::parse("rw,repair").unwrap().repair);
    assert_eq!(
        EfsOptions::parse("commit=0").unwrap().commit,
        Some(super::super::eclipsefs_mount::DEFAULT_COMMIT_SECS)
    );
    for bad in [
        "compress=brotli",
        "subvolid=x",
        "subvol",
        "device=",
        "commit",
        "commit=soon",
        "repair=yes",
    ] {
        assert!(EfsOptions::parse(bad).is_err(), "{}", bad);
    }
    assert_eq!(EfsOptions::parse("").unwrap(), EfsOptions::default());
}

#[test]
fn mount_option_repair_runs_the_repair() {
    let disk = formatted(16 << 20);
    {
        let fs = mount(&disk);
        file_with(&fs.root_inode(), "kept", b"still here");
        fs.sync().unwrap();
    }
    // Read-only: nothing may be written, so it is refused.
    let opts = EfsOptions::parse("repair").unwrap();
    assert!(EclipseMountFs::open_with(&MountBackend::Block(disk.clone()), true, &opts).is_err());
    let fs = mount_with(&disk, false, "repair");
    let f = fs.root_inode().find("kept").unwrap();
    assert_eq!(read_all(&f), b"still here");
    assert!(fs.inner.lock().check().is_ok());
}

#[test]
fn subvol_mounts_a_subvolume_as_the_root() {
    let disk = formatted(16 << 20);
    {
        let fs = mount(&disk);
        let root = fs.root_inode();
        let ctx = Ctx::root();
        let top = ctx.open(root.clone(), OpenFlags::RDONLY);
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "@home"))
            .unwrap();
        file_with(&root.find("@home").unwrap(), "inside", b"yes");
        fs.sync().unwrap();
    }
    let fs = mount_with(&disk, false, "subvol=/@home");
    assert_eq!(read_all(&fs.root_inode().find("inside").unwrap()), b"yes");
    let opts = EfsOptions::parse("subvol=/missing").unwrap();
    assert!(EclipseMountFs::open_with(&MountBackend::Block(disk), false, &opts).is_err());
}

#[test]
fn mount_compress_compresses_new_data() {
    let disk = formatted(32 << 20);
    let fs = mount_with(&disk, false, "compress=zstd");
    let f = file_with(&fs.root_inode(), "z", &vec![b'a'; 1 << 20]);
    fs.sync().unwrap();
    let ino = f
        .as_any_ref()
        .downcast_ref::<EclipseMountINode>()
        .unwrap()
        .ino;
    let size = fs.inner.lock().compsize(ino).unwrap();
    assert!(size.disk_bytes < size.uncompressed_bytes / 4, "{:?}", size);
}

/// `n` RAM disks formatted as one volume with filesystem uuid `[tag; 16]`.
fn volume(n: usize, tag: u8, profile: Profile) -> Vec<Arc<RamDisk>> {
    let disks: Vec<Arc<RamDisk>> = (0..n).map(|_| blank(48 << 20)).collect();
    let devs: Vec<Arc<dyn eclipsefs::BlockDevice>> = disks
        .iter()
        .map(|d| Arc::new(Formatter(d.clone())) as Arc<dyn eclipsefs::BlockDevice>)
        .collect();
    let opts = eclipsefs::mkfs::MkfsOptions {
        label: "VOL".into(),
        fsid: [tag; 16],
        chunk_uuid: [0; 16],
        dev_uuid: [0; 16],
        subvol_uuid: [0; 16],
        now: (1_700_000_000, 0),
    };
    eclipsefs::mkfs::format_devices(&devs, &opts, profile, profile).unwrap();
    disks
}

#[test]
fn a_volume_is_put_together_from_the_registered_devices() {
    let disks = volume(2, 0xa1, Profile::Raid1);
    for (i, d) in disks.iter().enumerate() {
        let block: Arc<dyn BlockScheme> = d.clone();
        assert!(probe_member(&block), "device {} passes the probe", i);
        register_block_device(&format!("efs-vol-a1-{}", i), block);
    }
    let data = pattern(500 << 10, 21);
    {
        // Mount through the SECOND device: the first is found by scan.
        let fs = mount(&disks[1]);
        assert_eq!(fs.inner.lock().devices().len(), 2);
        file_with(&fs.root_inode(), "mirrored", &data);
        fs.sync().unwrap();
        let ctx = Ctx::root();
        let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
        let mut info = vec![0u8; 1024];
        ctx.ioctl(fd, BTRFS_IOC_FS_INFO, &mut info).unwrap();
        assert_eq!((get64(&info, 0), get64(&info, 8)), (2, 2));
        let mut dev = vec![0u8; 4096];
        put64(&mut dev, 0, 2);
        ctx.ioctl(fd, BTRFS_IOC_DEV_INFO, &mut dev).unwrap();
        assert_eq!(get64(&dev, 32), 48 << 20);
    }
    let fs = mount(&disks[0]);
    assert_eq!(read_all(&fs.root_inode().find("mirrored").unwrap()), data);
}

fn probe_member(block: &Arc<dyn BlockScheme>) -> bool {
    super::super::eclipsefs_mount::probe_eclipsefs_superblock(block)
}

#[test]
fn a_volume_with_a_device_missing_mounts_only_degraded() {
    let disks = volume(2, 0xa2, Profile::Raid1);
    for (i, d) in disks.iter().enumerate() {
        let block: Arc<dyn BlockScheme> = d.clone();
        register_block_device(&format!("efs-vol-a2-{}", i), block);
    }
    {
        let fs = mount(&disks[0]);
        file_with(&fs.root_inode(), "kept", b"both copies");
        fs.sync().unwrap();
    }
    // The second device goes away.
    forget_block_device("efs-vol-a2-1");
    let opts = EfsOptions::default();
    assert_eq!(
        EclipseMountFs::open_with(&MountBackend::Block(disks[0].clone()), false, &opts).err(),
        Some(FsError::InvalidParam)
    );
    let fs = mount_with(&disks[0], true, "degraded");
    assert_eq!(
        read_all(&fs.root_inode().find("kept").unwrap()),
        b"both copies"
    );
}

#[test]
fn parity_and_tiers_by_ioctl() {
    let disks = volume(3, 0xa5, Profile::Raid5);
    for (i, d) in disks.iter().enumerate() {
        let block: Arc<dyn BlockScheme> = d.clone();
        register_block_device(&format!("efs-vol-a5-{}", i), block);
    }
    let data = pattern(2 << 20, 25);
    {
        let fs = mount(&disks[0]);
        assert_eq!(fs.inner.lock().profiles(), (Profile::Raid5, Profile::Raid5));
        file_with(&fs.root_inode(), "striped", &data);
        fs.sync().unwrap();
        let ctx = Ctx::root();
        let top = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
        // btrfs balance start -dconvert=raid6 (three devices are enough)
        let mut bal = vec![0u8; 1024];
        put64(&mut bal, 0, BTRFS_BALANCE_DATA);
        put64(&mut bal, 16 + 56, BTRFS_BLOCK_GROUP_RAID6);
        put64(&mut bal, 16 + 64, BTRFS_BALANCE_ARGS_CONVERT);
        ctx.ioctl(top, BTRFS_IOC_BALANCE_V2, &mut bal).unwrap();
        assert_eq!(fs.inner.lock().profiles(), (Profile::Raid6, Profile::Raid5));
        // Device 1 fast, the others slow.
        for (devid, tier) in [(1u64, 1u64), (2, 2), (3, 2)] {
            let mut t = vec![0u8; 32];
            put64(&mut t, 0, devid);
            put64(&mut t, 8, tier);
            ctx.ioctl(top, EFS_IOC_SET_TIER, &mut t).unwrap();
        }
        let mut t = vec![0u8; 32];
        put64(&mut t, 0, 1);
        ctx.ioctl(top, EFS_IOC_GET_TIER, &mut t).unwrap();
        assert_eq!(get64(&t, 8), 1);
        put64(&mut t, 0, 9);
        assert_eq!(
            ctx.ioctl(top, EFS_IOC_GET_TIER, &mut t),
            Err(LxError::ENODEV)
        );
        let mut bad = vec![0u8; 32];
        put64(&mut bad, 0, 1);
        put64(&mut bad, 8, 7);
        assert_eq!(
            ctx.ioctl(top, EFS_IOC_SET_TIER, &mut bad),
            Err(LxError::EINVAL)
        );
        // Only two slow devices: RAID6 data cannot live on them alone,
        // so it stays put; nothing breaks.
        let mut mig = vec![0u8; 32];
        put64(&mut mig, 0, EFS_TIER_MIGRATE_ALL);
        ctx.ioctl(top, EFS_IOC_TIER_MIGRATE, &mut mig).unwrap();
        assert_eq!(read_all(&fs.root_inode().find("striped").unwrap()), data);
        fs.sync().unwrap();
    }
    // Lose a device: parity has the rest.
    forget_block_device("efs-vol-a5-2");
    let fs = mount_with(&disks[0], true, "degraded");
    assert_eq!(read_all(&fs.root_inode().find("striped").unwrap()), data);
    assert_eq!(fs.inner.lock().device_tier(1), Tier::Fast);
    forget_block_device("efs-vol-a5-0");
    forget_block_device("efs-vol-a5-1");
}

#[test]
fn devices_are_added_balanced_and_removed_by_ioctl() {
    let disk = formatted(48 << 20);
    let fs = mount(&disk);
    let data = pattern(1 << 20, 23);
    file_with(&fs.root_inode(), "payload", &data);
    // Bigger than the first: a mirror of all of it has to fit.
    let extra = blank(64 << 20);
    let extra_block: Arc<dyn BlockScheme> = extra.clone();
    let node: Arc<dyn INode> = Arc::new(super::super::devfs::BlockDev::new(
        200,
        extra_block,
        "efs-extra".to_string(),
    ));
    let mut ctx = Ctx::root();
    ctx.devices.push((String::from("/dev/efs-extra"), node));
    let top = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
    ctx.ioctl(top, BTRFS_IOC_ADD_DEV, &mut vol_args(0, "/dev/efs-extra"))
        .unwrap();
    assert_eq!(fs.inner.lock().devices().len(), 2);
    // Adding it twice is refused.
    assert_eq!(
        ctx.ioctl(top, BTRFS_IOC_ADD_DEV, &mut vol_args(0, "/dev/efs-extra")),
        Err(LxError::EEXIST)
    );
    // btrfs balance start -dconvert=raid1 -mconvert=raid1
    let mut bal = vec![0u8; 1024];
    put64(&mut bal, 0, BTRFS_BALANCE_DATA | BTRFS_BALANCE_METADATA);
    for at in [16, 152] {
        put64(&mut bal, at + 56, BTRFS_BLOCK_GROUP_RAID1);
        put64(&mut bal, at + 64, BTRFS_BALANCE_ARGS_CONVERT);
    }
    ctx.ioctl(top, BTRFS_IOC_BALANCE_V2, &mut bal).unwrap();
    assert!(get64(&bal, 440) > 0, "chunks were moved");
    assert_eq!(fs.inner.lock().profiles(), (Profile::Raid1, Profile::Raid1));
    // RAID1 cannot drop to one device.
    let mut rm = vol_args_v2(0, BTRFS_DEVICE_SPEC_BY_ID, "");
    put64(&mut rm, 56, 2);
    assert!(ctx.ioctl(top, BTRFS_IOC_RM_DEV_V2, &mut rm).is_err());
    for at in [16, 152] {
        put64(&mut bal, at + 56, BTRFS_AVAIL_ALLOC_BIT_SINGLE);
    }
    ctx.ioctl(top, BTRFS_IOC_BALANCE_V2, &mut bal).unwrap();
    // Removed by path this time.
    ctx.ioctl(top, BTRFS_IOC_RM_DEV, &mut vol_args(0, "/dev/efs-extra"))
        .unwrap();
    assert_eq!(fs.inner.lock().devices().len(), 1);
    assert_eq!(read_all(&src_named(&fs, "payload")), data);
    // A combination of profiles is not one.
    put64(
        &mut bal,
        16 + 56,
        BTRFS_BLOCK_GROUP_RAID5 | BTRFS_BLOCK_GROUP_RAID1,
    );
    assert_eq!(
        ctx.ioctl(top, BTRFS_IOC_BALANCE_V2, &mut bal),
        Err(LxError::EINVAL)
    );
}

/// `struct fscrypt_key_specifier` naming key `id`.
fn key_spec(id: &[u8; 16], size: usize) -> Vec<u8> {
    let mut b = vec![0u8; size];
    put32(&mut b, 0, FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER);
    b[8..24].copy_from_slice(id);
    b
}

/// `FS_IOC_ADD_ENCRYPTION_KEY` with raw key `key`; the identifier.
fn add_key(ctx: &Ctx, fd: i32, key: &[u8]) -> LxResult<[u8; 16]> {
    let mut b = vec![0u8; FSCRYPT_ADD_KEY_ARG + key.len()];
    put32(&mut b, 0, FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER);
    put32(&mut b, 40, key.len() as u32);
    b[FSCRYPT_ADD_KEY_ARG..].copy_from_slice(key);
    ctx.ioctl(fd, FS_IOC_ADD_ENCRYPTION_KEY, &mut b)?;
    Ok(b[8..24].try_into().unwrap())
}

/// `struct fscrypt_policy_v2` (AES-256-XTS, AES-256-CTS, 32-byte
/// name padding).
fn policy_v2(id: &[u8; 16]) -> Vec<u8> {
    let mut b = vec![0u8; FSCRYPT_POLICY_V2_SIZE];
    b[..4].copy_from_slice(&[FSCRYPT_POLICY_V2, 1, 4, 3]);
    b[8..24].copy_from_slice(id);
    b
}

fn key_status(ctx: &Ctx, fd: i32, id: &[u8; 16]) -> u32 {
    let mut b = key_spec(id, 128);
    ctx.ioctl(fd, FS_IOC_GET_ENCRYPTION_KEY_STATUS, &mut b)
        .unwrap();
    get32(&b, 64)
}

#[test]
fn the_fscrypt_numbers_are_the_ones_linux_uses() {
    const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
        dir << 30 | size << 16 | (b'f' as u32) << 8 | nr
    }
    const W: u32 = 1;
    const R: u32 = 2;
    assert_eq!(FS_IOC_SET_ENCRYPTION_POLICY, ioc(R, 19, 12));
    assert_eq!(FS_IOC_GET_ENCRYPTION_POLICY, ioc(W, 21, 12));
    assert_eq!(FS_IOC_GET_ENCRYPTION_POLICY_EX, ioc(R | W, 22, 9));
    assert_eq!(FS_IOC_ADD_ENCRYPTION_KEY, ioc(R | W, 23, 80));
    assert_eq!(FS_IOC_REMOVE_ENCRYPTION_KEY, ioc(R | W, 24, 64));
    assert_eq!(FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS, ioc(R | W, 25, 64));
    assert_eq!(FS_IOC_GET_ENCRYPTION_KEY_STATUS, ioc(R | W, 26, 128));
    assert_eq!(FS_IOC_GET_ENCRYPTION_NONCE, ioc(R, 27, 16));
    assert_eq!(LxError::ENOKEY as isize, 126);
}

#[test]
fn an_encrypted_directory_locks_and_unlocks_like_fscrypt() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let ctx = Ctx::root();
    let top = ctx.open(root.clone(), OpenFlags::RDONLY);
    let key = [0x33u8; 64];
    let id = add_key(&ctx, top, &key).unwrap();
    assert_eq!(key_status(&ctx, top, &id), FSCRYPT_KEY_STATUS_PRESENT);

    let vault = root.create("vault", FileType::Dir, 0o700).unwrap();
    let vfd = ctx.open(vault.clone(), OpenFlags::RDONLY);
    ctx.ioctl(vfd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id))
        .unwrap();
    let mut ex = vec![0u8; 8 + FSCRYPT_POLICY_V2_SIZE];
    put64(&mut ex, 0, FSCRYPT_POLICY_V2_SIZE as u64);
    ctx.ioctl(vfd, FS_IOC_GET_ENCRYPTION_POLICY_EX, &mut ex)
        .unwrap();
    assert_eq!(get64(&ex, 0), FSCRYPT_POLICY_V2_SIZE as u64);
    assert_eq!(ex[8..], policy_v2(&id)[..]);
    // The old call cannot describe a v2 policy.
    assert_eq!(
        ctx.ioctl(vfd, FS_IOC_GET_ENCRYPTION_POLICY, &mut [0u8; 12]),
        Err(LxError::EINVAL)
    );
    let mut flags = [0u8; 4];
    ctx.ioctl(vfd, FS_IOC_GETFLAGS, &mut flags).unwrap();
    assert_ne!(u32::from_le_bytes(flags) & 0x800, 0);

    let data = pattern(20_000, 7);
    let f = file_with(&vault, "secret.txt", &data);
    let ffd = ctx.open(f.clone(), OpenFlags::RDONLY);
    let mut nonce = [0u8; 16];
    ctx.ioctl(ffd, FS_IOC_GET_ENCRYPTION_NONCE, &mut nonce)
        .unwrap();
    let mut dir_nonce = [0u8; 16];
    ctx.ioctl(vfd, FS_IOC_GET_ENCRYPTION_NONCE, &mut dir_nonce)
        .unwrap();
    assert_ne!(nonce, dir_nonce);
    assert_eq!(names(&vault), ["secret.txt"]);

    // Lock: names become no-key names, the data and new names need the
    // key.
    let mut rm = key_spec(&id, 64);
    ctx.ioctl(top, FS_IOC_REMOVE_ENCRYPTION_KEY, &mut rm)
        .unwrap();
    assert_eq!(get32(&rm, 40), 0);
    assert_eq!(key_status(&ctx, top, &id), FSCRYPT_KEY_STATUS_ABSENT);
    let hidden = names(&vault);
    assert_eq!(hidden.len(), 1);
    assert_ne!(hidden[0], "secret.txt");
    assert!(vault.find(&hidden[0]).is_ok());
    let mut buf = [0u8; 16];
    assert_eq!(f.read_at(0, &mut buf), Err(FsError::NoKey));
    assert_eq!(
        vault.create("new", FileType::File, 0o644).err(),
        Some(FsError::NoKey)
    );
    assert_eq!(
        LxError::from(FsError::NoKey),
        LxError::ENOKEY,
        "userspace sees ENOKEY"
    );
    assert_eq!(
        ctx.ioctl(top, FS_IOC_REMOVE_ENCRYPTION_KEY, &mut key_spec(&id, 64)),
        Err(LxError::ENOKEY)
    );

    // Unlock: everything is back.
    assert_eq!(add_key(&ctx, top, &key).unwrap(), id);
    assert_eq!(names(&vault), ["secret.txt"]);
    assert_eq!(read_all(&vault.find("secret.txt").unwrap()), data);
    fs.sync().unwrap();
    drop(fs);
    // A new mount starts locked.
    let fs = mount(&disk);
    let vault = fs.root_inode().find("vault").unwrap();
    assert!(vault.find("secret.txt").is_err());
}

#[test]
fn fscrypt_ioctls_refuse_what_linux_refuses() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let root = fs.root_inode();
    let ctx = Ctx::root();
    let top = ctx.open(root.clone(), OpenFlags::RDONLY);
    let id = add_key(&ctx, top, &[0x44u8; 32]).unwrap();
    // Keys too short, from a keyring, or named by a v1 descriptor.
    assert_eq!(add_key(&ctx, top, &[1u8; 8]), Err(LxError::EINVAL));
    let mut b = vec![0u8; FSCRYPT_ADD_KEY_ARG + 32];
    put32(&mut b, 0, FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER);
    put32(&mut b, 40, 32);
    put32(&mut b, 44, 1234);
    assert_eq!(
        ctx.ioctl(top, FS_IOC_ADD_ENCRYPTION_KEY, &mut b),
        Err(LxError::EINVAL)
    );
    let mut v1 = key_spec(&id, 64);
    put32(&mut v1, 0, 1);
    assert_eq!(
        ctx.ioctl(top, FS_IOC_REMOVE_ENCRYPTION_KEY, &mut v1),
        Err(LxError::EINVAL)
    );

    // Plain inodes have no policy and no nonce.
    let mut ex = vec![0u8; 8 + FSCRYPT_POLICY_V2_SIZE];
    put64(&mut ex, 0, FSCRYPT_POLICY_V2_SIZE as u64);
    assert_eq!(
        ctx.ioctl(top, FS_IOC_GET_ENCRYPTION_POLICY_EX, &mut ex),
        Err(LxError::ENODATA)
    );
    assert_eq!(
        ctx.ioctl(top, FS_IOC_GET_ENCRYPTION_NONCE, &mut [0u8; 16]),
        Err(LxError::ENODATA)
    );

    // A v1 policy, a full directory, a file, a missing key, a stranger.
    let full = root.create("full", FileType::Dir, 0o755).unwrap();
    file_with(&full, "x", b"");
    let full = ctx.open(full, OpenFlags::RDONLY);
    let mut v1 = vec![0u8; 12];
    v1[1] = 1;
    v1[2] = 4;
    assert_eq!(
        ctx.ioctl(full, FS_IOC_SET_ENCRYPTION_POLICY, &mut v1),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        ctx.ioctl(full, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id)),
        Err(LxError::ENOTEMPTY)
    );
    let f = ctx.open(file_with(&root, "f", b""), OpenFlags::RDONLY);
    assert_eq!(
        ctx.ioctl(f, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id)),
        Err(LxError::ENOTDIR)
    );
    let empty = root.create("empty", FileType::Dir, 0o777).unwrap();
    let efd = ctx.open(empty.clone(), OpenFlags::RDONLY);
    assert_eq!(
        ctx.ioctl(efd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&[9; 16])),
        Err(LxError::ENOKEY)
    );
    let stranger = Ctx::as_user(1000);
    let sfd = stranger.open(empty.clone(), OpenFlags::RDONLY);
    assert_eq!(
        stranger.ioctl(sfd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id)),
        Err(LxError::EACCES)
    );
    assert_eq!(
        stranger.ioctl(
            sfd,
            FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS,
            &mut key_spec(&id, 64)
        ),
        Err(LxError::EACCES)
    );
    // Once set, the same policy is fine and another one is EEXIST.
    ctx.ioctl(efd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id))
        .unwrap();
    ctx.ioctl(efd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id))
        .unwrap();
    let other = add_key(&ctx, top, &[0x55u8; 32]).unwrap();
    assert_eq!(
        ctx.ioctl(efd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&other)),
        Err(LxError::EEXIST)
    );
    ctx.ioctl(
        top,
        FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS,
        &mut key_spec(&id, 64),
    )
    .unwrap();
}

/// Every error the crate raises has one errno at this boundary, and
/// btrfs-progs reads them: `EOPNOTSUPP` is "this filesystem cannot",
/// `ENOKEY` is "unlock it first", and `EIO` is the one that means the
/// disk. Four of the twelve arms answered for nothing until now.
#[test]
fn the_twelve_errors_of_the_crate_keep_their_meaning_at_the_ioctl_boundary() {
    let table = [
        (EfsError::Io, LxError::EIO),
        (EfsError::BadSuperblock, LxError::EINVAL),
        (EfsError::Corrupt("a torn node"), LxError::EIO),
        (
            EfsError::Unsupported("a format from the future"),
            LxError::EOPNOTSUPP,
        ),
        (EfsError::NotFound, LxError::ENOENT),
        (EfsError::Exists, LxError::EEXIST),
        (EfsError::NotDir, LxError::ENOTDIR),
        (EfsError::IsDir, LxError::EISDIR),
        (EfsError::NotEmpty, LxError::ENOTEMPTY),
        (EfsError::NoSpace, LxError::ENOSPC),
        (EfsError::Invalid, LxError::EINVAL),
        (EfsError::NoKey, LxError::ENOKEY),
    ];
    for (e, want) in table {
        assert_eq!(errno(e), want, "{:?}", e);
    }
    // A corrupt filesystem and a bad superblock are different things to
    // say, so they do not share an errno: the first is EIO (the disk
    // says one thing and the tree another), the second EINVAL (this is
    // not an EclipseFS at all).
    assert_ne!(errno(EfsError::Corrupt("")), errno(EfsError::BadSuperblock));
}

/// A name field that holds its NUL but is not UTF-8 is EINVAL: it is not
/// too long, it just does not say a name. The two errors are what tell a
/// truncated field from a mangled one.
#[test]
fn a_name_that_is_not_utf8_is_einval_and_not_enametoolong() {
    let disk = formatted(16 << 20);
    let fs = mount(&disk);
    let ctx = Ctx::root();
    let top = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
    let mut args = vol_args(0, "");
    // A lone continuation byte and a byte no UTF-8 sequence starts with.
    args[8..11].copy_from_slice(&[0x80, 0xff, 0x00]);
    assert_eq!(
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut args),
        Err(LxError::EINVAL)
    );
    // The same bytes with no NUL in the whole field are the other error.
    let mut unterminated = vec![0x80u8; 4096];
    unterminated[0..8].fill(0);
    assert_eq!(
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut unterminated),
        Err(LxError::ENAMETOOLONG)
    );
}

/// The four names a subvolume cannot have are refused BEFORE the mount's
/// writability and before the caller's capabilities. That order is the
/// whole of what the check here buys: `check_name` one layer down
/// refuses the same four, so on a writable mount and as root the answer
/// is EINVAL either way -- what changes is the answer a read-only mount
/// and an unprivileged caller get, and btrfs-progs prints it.
#[test]
fn a_subvolume_name_is_judged_before_the_mount_and_before_the_caller() {
    let disk = formatted(16 << 20);
    {
        let fs = mount(&disk);
        fs.root_inode().create("sv", FileType::Dir, 0o755).unwrap();
        fs.sync().unwrap();
    }
    let fs = mount_with(&disk, true, "");
    let root = fs.root_inode();
    let ctx = Ctx::root();
    let top = ctx.open(root.clone(), OpenFlags::RDONLY);
    for bad in ["", ".", "..", "a/b"] {
        assert_eq!(
            ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, bad)),
            Err(LxError::EINVAL),
            "{:?} on a read-only mount",
            bad
        );
    }
    // A name that is a name is where EROFS comes from, which is what
    // makes the four above a verdict on the name and not on the mount.
    assert_eq!(
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "new")),
        Err(LxError::EROFS)
    );
    // Deleting reads the name before it asks for CAP_SYS_ADMIN.
    let user = Ctx::as_user(1000);
    let utop = user.open(root, OpenFlags::RDONLY);
    for bad in ["", ".", "..", "a/b"] {
        assert_eq!(
            user.ioctl(utop, BTRFS_IOC_SNAP_DESTROY, &mut vol_args(0, bad)),
            Err(LxError::EINVAL),
            "{:?} as an unprivileged caller",
            bad
        );
    }
    assert_eq!(
        user.ioctl(utop, BTRFS_IOC_SNAP_DESTROY, &mut vol_args(0, "sv")),
        Err(LxError::EPERM)
    );
}

/// A string that exactly fills its field leaves no room for the
/// terminator, so it is already too long: the field is `max` bytes
/// counting the NUL, the way every btrfs name field is.
#[test]
fn a_name_that_fills_its_field_leaves_no_room_for_the_terminator() {
    let mut b = [0xffu8; 16];
    assert_eq!(put_cstr(&mut b, 4, 4, "abcd"), Err(LxError::ENAMETOOLONG));
    assert_eq!(b, [0xffu8; 16], "a refused name wrote something anyway");
    // One byte shorter fits, NUL and all, and nothing past the field is
    // touched.
    put_cstr(&mut b, 4, 4, "abc").unwrap();
    assert_eq!(&b[3..9], b"\xffabc\0\xff");
}

/// Every suffix the kernel's `memparse` takes, ten bits apart, in both
/// the cases btrfs-progs writes them in.
#[test]
fn the_size_suffixes_are_the_kernels_powers_of_1024() {
    let table = [
        ("", 0u32),
        ("k", 10),
        ("m", 20),
        ("g", 30),
        ("t", 40),
        ("p", 50),
        ("e", 60),
    ];
    for (suffix, shift) in table {
        let lower = format!("3{}", suffix);
        let upper = format!("3{}", suffix.to_uppercase());
        assert_eq!(memparse(&lower), Some(3u64 << shift), "{:?}", lower);
        assert_eq!(memparse(&upper), Some(3u64 << shift), "{:?}", upper);
    }
    // Two suffixes, or a suffix of its own, is not a size.
    assert_eq!(memparse("3kk"), None);
    assert_eq!(memparse("3 k"), None);
}

/// The gate and the numbers at once: every ioctl this module answers is
/// called by the number Linux and btrfs-progs call it by, and
/// `is_fs_ioctl` lets that number in.
///
/// Both halves matter, and the gate half is the one nothing else here
/// checks: the tests of this module reach `Target` directly, the way the
/// syscall layer does only after `eclipsefs_ioctl` has asked the gate.
/// So an entry dropped from the gate list -- which makes the ioctl fall
/// through to the inode and answer ENOTTY -- leaves every other test in
/// this file green.
#[test]
fn the_gate_lets_in_every_number_this_module_answers() {
    const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
        dir << 30 | size << 16 | ty << 8 | nr
    }
    const W: u32 = 1;
    const R: u32 = 2;
    // The ones `the_ioctl_numbers_are_the_ones_linux_and_btrfs_progs_use`
    // and `the_fscrypt_numbers_are_the_ones_linux_uses` leave out, plus
    // FICLONERANGE, whose number is checked there but whose place in the
    // gate list was not.
    let table = [
        ("FS_IOC32_GETFLAGS", ioc(R, b'f' as u32, 1, 4)),
        ("FS_IOC32_SETFLAGS", ioc(W, b'f' as u32, 2, 4)),
        ("FICLONERANGE", ioc(W, 0x94, 13, 32)),
        ("BTRFS_IOC_SNAP_CREATE", ioc(W, 0x94, 1, 4096)),
        ("BTRFS_IOC_SYNC", ioc(0, 0x94, 8, 0)),
        ("BTRFS_IOC_ADD_DEV", ioc(W, 0x94, 10, 4096)),
        ("BTRFS_IOC_RM_DEV", ioc(W, 0x94, 11, 4096)),
        ("BTRFS_IOC_SUBVOL_CREATE", ioc(W, 0x94, 14, 4096)),
        ("BTRFS_IOC_SNAP_DESTROY", ioc(W, 0x94, 15, 4096)),
        ("BTRFS_IOC_INO_LOOKUP", ioc(R | W, 0x94, 18, 4096)),
        ("BTRFS_IOC_DEFAULT_SUBVOL", ioc(W, 0x94, 19, 8)),
        ("BTRFS_IOC_WAIT_SYNC", ioc(W, 0x94, 22, 8)),
        ("BTRFS_IOC_SUBVOL_GETFLAGS", ioc(R, 0x94, 25, 8)),
        ("BTRFS_IOC_SUBVOL_SETFLAGS", ioc(W, 0x94, 26, 8)),
        ("BTRFS_IOC_SCRUB_CANCEL", ioc(0, 0x94, 28, 0)),
        ("BTRFS_IOC_SCRUB_PROGRESS", ioc(R | W, 0x94, 29, 1024)),
        ("BTRFS_IOC_BALANCE_CTL", ioc(W, 0x94, 33, 4)),
        ("BTRFS_IOC_BALANCE_PROGRESS", ioc(R, 0x94, 34, 1024)),
        ("BTRFS_IOC_QUOTA_CTL", ioc(R | W, 0x94, 40, 16)),
        ("BTRFS_IOC_QUOTA_RESCAN", ioc(W, 0x94, 44, 64)),
        ("BTRFS_IOC_QUOTA_RESCAN_STATUS", ioc(R, 0x94, 45, 64)),
        ("BTRFS_IOC_QUOTA_RESCAN_WAIT", ioc(0, 0x94, 46, 0)),
        ("BTRFS_IOC_GET_FEATURES", ioc(R, 0x94, 57, 24)),
        ("BTRFS_IOC_RM_DEV_V2", ioc(W, 0x94, 58, 4096)),
    ];
    for (name, cmd) in table {
        assert!(is_fs_ioctl(cmd), "{} does not reach this module", name);
    }
    // And the ones this module does not answer stay out, so the gate is
    // a list and not a `true`.
    for (name, cmd) in [
        ("FS_IOC_GETVERSION", ioc(R, b'v' as u32, 1, 8)),
        ("BTRFS_IOC_TRANS_START", ioc(0, 0x94, 6, 0)),
        ("TIOCGWINSZ", 0x5413),
    ] {
        assert!(!is_fs_ioctl(cmd), "{} is not ours", name);
    }
}

/// The nine profiles `btrfs balance -dconvert=` names, as the nine bit
/// patterns it sends. btrfs-progs sets the "available" bit alongside the
/// profile, and that bit names no profile of its own.
#[test]
fn the_nine_balance_profiles_are_the_nine_btrfs_bit_patterns() {
    let table = [
        (0, Profile::Single),
        (BTRFS_BLOCK_GROUP_DUP, Profile::Dup),
        (BTRFS_BLOCK_GROUP_RAID0, Profile::Raid0),
        (BTRFS_BLOCK_GROUP_RAID1, Profile::Raid1),
        (BTRFS_BLOCK_GROUP_RAID10, Profile::Raid10),
        (BTRFS_BLOCK_GROUP_RAID1C3, Profile::Raid1c3),
        (BTRFS_BLOCK_GROUP_RAID1C4, Profile::Raid1c4),
        (BTRFS_BLOCK_GROUP_RAID5, Profile::Raid5),
        (BTRFS_BLOCK_GROUP_RAID6, Profile::Raid6),
    ];
    for (bits, want) in table {
        assert_eq!(profile(bits), Ok(want), "{:#x}", bits);
        assert_eq!(
            profile(bits | BTRFS_AVAIL_ALLOC_BIT_SINGLE),
            Ok(want),
            "{:#x} with the available bit",
            bits
        );
    }
    // Two profiles at once is not a profile.
    assert_eq!(
        profile(BTRFS_BLOCK_GROUP_RAID1 | BTRFS_BLOCK_GROUP_RAID10),
        Err(LxError::EINVAL)
    );
}

/// A punched or zeroed range leaves nothing of itself in the page cache.
///
/// The pages have to be IN the cache before the call, which is why
/// nothing saw this: a cache nobody has read holds nothing to go stale,
/// and a read after the hole fills it from the filesystem, which already
/// says zeros. So the test reads first, punches second and reads the
/// cache again, and both modes that change bytes under it count --
/// PUNCH_HOLE and ZERO_RANGE alike.
#[test]
fn a_zeroed_or_punched_range_leaves_nothing_behind_in_the_page_cache() {
    let disk = formatted(32 << 20);
    let fs = mount(&disk);
    let data = pattern(64 << 10, 23);
    let inode = file_with(&fs.root_inode(), "f", &data);
    let file = File::new(inode.clone(), OpenFlags::RDWR, String::from("/efs-test/f"));
    let (vmo, _) = file.get_vmo_shared(0, data.len()).unwrap();
    let mut seen = vec![0u8; 8192];
    // This read is what fills the cache, and without it the two modes
    // below are indistinguishable.
    vmo.read(8192, &mut seen).unwrap();
    assert_eq!(&seen[..], &data[8192..16384], "the page cache did not fill");

    // FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE.
    eclipsefs_fallocate(&file, 0x11, 8192, 4096)
        .unwrap()
        .unwrap();
    vmo.read(8192, &mut seen).unwrap();
    assert!(
        seen[..4096].iter().all(|&b| b == 0),
        "the cache still holds what a zeroed range gave up"
    );
    assert_eq!(&seen[4096..], &data[12288..16384], "past the zeroed range");

    // FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE.
    eclipsefs_fallocate(&file, 0x3, 12288, 4096)
        .unwrap()
        .unwrap();
    vmo.read(12288, &mut seen[..4096]).unwrap();
    assert!(
        seen[..4096].iter().all(|&b| b == 0),
        "the cache still holds what a punched hole gave up"
    );
    // And the filesystem says the same thing, so the cache was not
    // zeroed on its own.
    let mut want = data;
    want[8192..16384].fill(0);
    assert_eq!(read_all(&inode), want);
}
