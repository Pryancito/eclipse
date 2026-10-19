//! `/dev/nvidia-uvm` (hatch `nvidia.uvm`, default off, and only beside
//! `/dev/nvidiactl`): the device file of NVIDIA's unified-memory driver, the
//! second one CUDA opens.
//!
//! On Linux it is a module of its own, kernel-open/nvidia-uvm. libcuda's
//! `cuInit` opens it twice and makes three calls; if the file is missing, or
//! any of the three fails, `cuInit` answers CUDA_ERROR_UNKNOWN (999) and no
//! CUDA program gets a device. Those three are served here as uvm.c serves
//! them:
//!
//! - UVM_INITIALIZE makes the open a VA space with the caller's flags. Asked
//!   again with the same flags it changes nothing; with others, or with a
//!   flag the module does not know, it is refused.
//! - UVM_MM_INITIALIZE checks that the fd it names is an initialised open of
//!   this file, then answers NV_WARN_NOTHING_TO_DO: there is no MMU notifier
//!   here to tie a VA space to the process's address space, which is the
//!   module's own answer when it runs with `uvm_enable_va_space_mm=0`, and
//!   libcuda closes the second open on it.
//! - UVM_PAGEABLE_MEM_ACCESS answers false: the GPU cannot reach ordinary
//!   process memory here (no HMM, no ATS).
//!
//! UVM_DEINITIALIZE succeeds and does nothing, as there. Every other command
//! is ENOSYS, the module's answer to one it does not know, and is logged by
//! name the first time it is asked for: that line says what the next batch
//! needs. The first is expected at `cuCtxCreate`, which registers the GPU
//! and its VA space with UVM, and from then on UVM owns that VA space's page
//! tables.
//!
//! A command is a plain number (`UVM_IOCTL_BASE(i)` is `i` on Linux), not an
//! `_IOC` encoding. A served one answers through its params' `rmStatus`, and
//! the ioctl itself fails only when the params cannot be copied. It arrives
//! from `sys_ioctl` (linux-syscall), like the escapes of `/dev/nvidiactl`,
//! because UVM_MM_INITIALIZE names a fd of the caller's.

use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use core::any::Any;

use kernel_hal::user::{UserInPtr, UserOutPtr};
use lock::Mutex;
use rcore_fs::vfs::*;
use rcore_fs_devfs::{DevFS, DevINode};
use zcore_drivers::display::nvctl;

use crate::error::{LxError, LxResult};

/// The major Linux hands nvidia-uvm is dynamic (`alloc_chrdev_region`), so
/// any free one does; this is a common one there.
pub const MAJOR: usize = 510;

/// The commands, from uvm_linux_ioctl.h and uvm_ioctl.h.
pub mod cmd {
    pub const INITIALIZE: u32 = 0x3000_0001;
    pub const DEINITIALIZE: u32 = 0x3000_0002;
    pub const PAGEABLE_MEM_ACCESS: u32 = 39;
    pub const MM_INITIALIZE: u32 = 75;
}

/// `UVM_INITIALIZE_PARAMS`: `NvU64 flags`, `NV_STATUS rmStatus`.
const INITIALIZE_SIZE: usize = 16;
const INITIALIZE_STATUS: usize = 8;
/// `UVM_MM_INITIALIZE_PARAMS`: `NvS32 uvmFd`, `NV_STATUS rmStatus`.
const MM_INITIALIZE_SIZE: usize = 8;
const MM_INITIALIZE_STATUS: usize = 4;
/// `UVM_PAGEABLE_MEM_ACCESS_PARAMS`: `NvBool pageableMemAccess`,
/// `NV_STATUS rmStatus`.
const PAGEABLE_MEM_ACCESS_SIZE: usize = 8;
const PAGEABLE_MEM_ACCESS_STATUS: usize = 4;

/// `UVM_INIT_FLAGS_MASK`: DISABLE_HMM and MULTI_PROCESS_SHARING_MODE.
const INIT_FLAGS_MASK: u64 = 0x3;

const NV_OK: u32 = 0;
const NV_ERR_ILLEGAL_ACTION: u32 = 0x16;
const NV_ERR_INVALID_ARGUMENT: u32 = 0x1F;
const NV_WARN_NOTHING_TO_DO: u32 = 0x0001_0006;

fn rdev() -> usize {
    make_rdev(MAJOR, 0)
}

fn metadata(inode_id: usize) -> Metadata {
    Metadata {
        dev: 1,
        inode: inode_id,
        size: 0,
        blk_size: 0,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_: FileType::CharDevice,
        // What nvidia-modprobe creates it as.
        mode: 0o666,
        nlinks: 1,
        uid: 0,
        gid: 0,
        rdev: rdev(),
    }
}

/// `/dev/nvidia-uvm` in the device filesystem. Opening it makes a
/// [`UvmOpen`], which is what the fd holds.
pub struct UvmDev {
    inode_id: usize,
}

impl UvmDev {
    fn new() -> Self {
        UvmDev {
            inode_id: DevFS::new_inode_id(),
        }
    }

    /// `uvm_open`: every open starts uninitialised, and is a VA space of its
    /// own once UVM_INITIALIZE makes it one.
    pub fn open_client(&self) -> Arc<dyn INode> {
        Arc::new(UvmOpen::new(self.inode_id))
    }
}

impl INode for UvmDev {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::InvalidParam)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::InvalidParam)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus::default())
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(metadata(self.inode_id))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// What an open is (`uvm_fd_type_t`). The module's UVM_FD_MM never comes
/// up: UVM_MM_INITIALIZE answers before it would make one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FdType {
    Uninitialized,
    VaSpace { flags: u64 },
}

/// One open of `/dev/nvidia-uvm`.
pub struct UvmOpen {
    inode_id: usize,
    fd_type: Mutex<FdType>,
}

impl UvmOpen {
    fn new(inode_id: usize) -> Self {
        UvmOpen {
            inode_id,
            fd_type: Mutex::new(FdType::Uninitialized),
        }
    }

    /// `uvm_fd_va_space`: whether UVM_INITIALIZE made this open a VA space.
    fn is_va_space(&self) -> bool {
        matches!(*self.fd_type.lock(), FdType::VaSpace { .. })
    }

    /// `uvm_ioctl`. `open_of_fd` is the inode behind one of the caller's
    /// fds, for UVM_MM_INITIALIZE.
    pub fn ioctl(
        &self,
        cmd: u32,
        arg: usize,
        open_of_fd: &dyn Fn(i32) -> Option<Arc<dyn INode>>,
    ) -> LxResult<usize> {
        let size = match cmd {
            cmd::DEINITIALIZE => return Ok(0),
            cmd::INITIALIZE => INITIALIZE_SIZE,
            cmd::MM_INITIALIZE => MM_INITIALIZE_SIZE,
            cmd::PAGEABLE_MEM_ACCESS => PAGEABLE_MEM_ACCESS_SIZE,
            _ => {
                not_served(cmd);
                return Err(LxError::ENOSYS);
            }
        };
        let mut params = UserInPtr::<u8>::from(arg)
            .read_array(size)
            .map_err(|_| LxError::EFAULT)?;
        self.call(cmd, &mut params, open_of_fd);
        UserOutPtr::<u8>::from(arg)
            .write_array(&params)
            .map_err(|_| LxError::EFAULT)?;
        Ok(0)
    }

    /// A served command on the kernel copy of its params: its answer goes in
    /// their `rmStatus`.
    fn call(
        &self,
        cmd: u32,
        params: &mut [u8],
        open_of_fd: &dyn Fn(i32) -> Option<Arc<dyn INode>>,
    ) {
        let (status, at) = match cmd {
            cmd::INITIALIZE => (self.initialize(u64_at(params, 0)), INITIALIZE_STATUS),
            cmd::MM_INITIALIZE => (
                mm_initialize(i32_at(params, 0), open_of_fd),
                MM_INITIALIZE_STATUS,
            ),
            cmd::PAGEABLE_MEM_ACCESS => {
                // The INIT_CHECK of the module's route: not on an open that
                // is not a VA space, whose params go back as they came.
                let status = if self.is_va_space() {
                    params[0] = 0;
                    NV_OK
                } else {
                    NV_ERR_ILLEGAL_ACTION
                };
                (status, PAGEABLE_MEM_ACCESS_STATUS)
            }
            _ => unreachable!("only served commands get here"),
        };
        params[at..at + 4].copy_from_slice(&status.to_le_bytes());
    }

    /// `uvm_api_initialize`.
    fn initialize(&self, flags: u64) -> u32 {
        let mut fd_type = self.fd_type.lock();
        match *fd_type {
            // `uvm_va_space_create`.
            FdType::Uninitialized if flags & !INIT_FLAGS_MASK != 0 => NV_ERR_INVALID_ARGUMENT,
            FdType::Uninitialized => {
                *fd_type = FdType::VaSpace { flags };
                NV_OK
            }
            FdType::VaSpace { flags: was } if was == flags => NV_OK,
            FdType::VaSpace { .. } => NV_ERR_INVALID_ARGUMENT,
        }
    }
}

/// `uvm_api_mm_initialize`, with no va_space_mm to initialise: the fd must
/// be an open of this file that is a VA space, and then there is nothing to
/// do. The open it is issued on is left as it was.
fn mm_initialize(uvm_fd: i32, open_of_fd: &dyn Fn(i32) -> Option<Arc<dyn INode>>) -> u32 {
    let is_va_space = open_of_fd(uvm_fd)
        .as_ref()
        .and_then(|inode| inode.downcast_ref::<UvmOpen>())
        .is_some_and(UvmOpen::is_va_space);
    if is_va_space {
        NV_WARN_NOTHING_TO_DO
    } else {
        NV_ERR_INVALID_ARGUMENT
    }
}

impl INode for UvmOpen {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::InvalidParam)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::InvalidParam)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus::default())
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(metadata(self.inode_id))
    }

    /// Never reached: `sys_ioctl` calls [`UvmOpen::ioctl`] with the caller's
    /// fds, which this signature has no room for.
    fn io_control(&self, _cmd: u32, _data: usize) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The commands uvm.c routes, by number, for the log.
fn name(cmd: u32) -> &'static str {
    match cmd {
        1 => "UVM_RESERVE_VA",
        2 => "UVM_RELEASE_VA",
        3 => "UVM_REGION_COMMIT",
        4 => "UVM_REGION_DECOMMIT",
        5 => "UVM_REGION_SET_STREAM",
        6 => "UVM_SET_STREAM_RUNNING",
        7 => "UVM_SET_STREAM_STOPPED",
        23 => "UVM_CREATE_RANGE_GROUP",
        24 => "UVM_DESTROY_RANGE_GROUP",
        25 => "UVM_REGISTER_GPU_VASPACE",
        26 => "UVM_UNREGISTER_GPU_VASPACE",
        27 => "UVM_REGISTER_CHANNEL",
        28 => "UVM_UNREGISTER_CHANNEL",
        29 => "UVM_ENABLE_PEER_ACCESS",
        30 => "UVM_DISABLE_PEER_ACCESS",
        31 => "UVM_SET_RANGE_GROUP",
        33 => "UVM_MAP_EXTERNAL_ALLOCATION",
        34 => "UVM_FREE",
        35 => "UVM_MEM_MAP",
        37 => "UVM_REGISTER_GPU",
        38 => "UVM_UNREGISTER_GPU",
        40 => "UVM_PREVENT_MIGRATION_RANGE_GROUPS",
        41 => "UVM_ALLOW_MIGRATION_RANGE_GROUPS",
        42 => "UVM_SET_PREFERRED_LOCATION",
        43 => "UVM_UNSET_PREFERRED_LOCATION",
        44 => "UVM_ENABLE_READ_DUPLICATION",
        45 => "UVM_DISABLE_READ_DUPLICATION",
        46 => "UVM_SET_ACCESSED_BY",
        47 => "UVM_UNSET_ACCESSED_BY",
        51 => "UVM_MIGRATE",
        53 => "UVM_MIGRATE_RANGE_GROUP",
        54 => "UVM_ENABLE_SYSTEM_WIDE_ATOMICS",
        55 => "UVM_DISABLE_SYSTEM_WIDE_ATOMICS",
        62 => "UVM_TOOLS_READ_PROCESS_MEMORY",
        63 => "UVM_TOOLS_WRITE_PROCESS_MEMORY",
        64 => "UVM_TOOLS_GET_PROCESSOR_UUID_TABLE",
        65 => "UVM_MAP_DYNAMIC_PARALLELISM_REGION",
        66 => "UVM_UNMAP_EXTERNAL",
        67 => "UVM_TOOLS_FLUSH_EVENTS",
        68 => "UVM_ALLOC_SEMAPHORE_POOL",
        69 => "UVM_CLEAN_UP_ZOMBIE_RESOURCES",
        70 => "UVM_PAGEABLE_MEM_ACCESS_ON_GPU",
        71 => "UVM_POPULATE_PAGEABLE",
        72 => "UVM_VALIDATE_VA_RANGE",
        73 => "UVM_CREATE_EXTERNAL_RANGE",
        74 => "UVM_MAP_EXTERNAL_SPARSE",
        77 => "UVM_TOOLS_GET_PROCESSOR_UUID_TABLE_V2",
        78 => "UVM_ALLOC_DEVICE_P2P",
        79 => "UVM_CLEAR_ALL_ACCESS_COUNTERS",
        80 => "UVM_DISCARD",
        _ => "unknown",
    }
}

/// The commands already logged, so each is logged once.
static NOT_SERVED: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

/// One log line the first time each command this kernel does not serve is
/// asked for. Every line of this file goes to dmesg through klog, not
/// `warn!`: a real machine boots at `LOG=error`, which filters `warn!` out,
/// and this line is what says what the next batch needs.
fn not_served(cmd: u32) {
    if NOT_SERVED.lock().insert(cmd) {
        zcore_drivers::klog_warn!(
            "[nvidia-uvm] {} ({}) is not served yet: ENOSYS",
            name(cmd),
            cmd
        );
    }
}

/// `/dev/nvidia-uvm`, with `nvidia.uvm` and once `/dev/nvidiactl` exists:
/// CUDA hands UVM the RM clients it made through that.
pub fn create_node(root: &Arc<DevINode>) {
    if !nvctl::uvm_enabled() {
        return;
    }
    if root.find("nvidiactl").is_err() {
        zcore_drivers::klog_warn!(
            "[nvidia-uvm] nvidia.uvm without /dev/nvidiactl (nvidia.cuda turns on both): no /dev/nvidia-uvm"
        );
        return;
    }
    if let Err(e) = root.add("nvidia-uvm", Arc::new(UvmDev::new())) {
        zcore_drivers::klog_warn!("[nvidia-uvm] failed to mknod /dev/nvidia-uvm: {:?}", e);
        return;
    }
    zcore_drivers::klog_warn!(
        "[nvidia-uvm] /dev/nvidia-uvm: cuInit's three calls served, the rest logged"
    );
}

#[cfg(test)]
mod tests;
