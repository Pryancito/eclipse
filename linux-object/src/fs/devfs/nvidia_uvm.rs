//! `/dev/nvidia-uvm` (hatch `nvidia.uvm`, default off, and only beside
//! `/dev/nvidiactl`): the device file of NVIDIA's unified-memory driver, the
//! second one CUDA opens.
//!
//! On Linux it is a module of its own, kernel-open/nvidia-uvm. libcuda's
//! `cuInit` opens it twice and makes three calls; if the file is missing, or
//! any of the three fails, `cuInit` answers CUDA_ERROR_UNKNOWN (999) and no
//! CUDA program gets a device. Once it has the devices it registers each
//! GPU with UVM and makes a range group, and if either fails `cuInit`
//! answers CUDA_ERROR_NOT_INITIALIZED (3). All of them are served here as
//! uvm.c serves them:
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
//! - UVM_REGISTER_GPU registers, in the VA space, a GPU the RM brought up,
//!   named by the UUID the RM hands a client; one it does not know, or one
//!   registered already, is refused. UVM_UNREGISTER_GPU undoes it.
//! - UVM_CREATE_RANGE_GROUP hands out a new range group id, which
//!   UVM_DESTROY_RANGE_GROUP takes back. Nothing is put in a group yet.
//!
//! UVM_DEINITIALIZE succeeds and does nothing, as there. Every other command
//! is ENOSYS, the module's answer to one it does not know, and is logged by
//! name the first time it is asked for: that line says what the next batch
//! needs. The first is expected at `cuCtxCreate`: UVM_REGISTER_GPU_VASPACE,
//! after which UVM owns that GPU VA space's page tables.
//!
//! A command is a plain number (`UVM_IOCTL_BASE(i)` is `i` on Linux), not an
//! `_IOC` encoding. A served one answers through its params' `rmStatus`, and
//! the ioctl itself fails only when the params cannot be copied. It arrives
//! from `sys_ioctl` (linux-syscall), like the escapes of `/dev/nvidiactl`,
//! because UVM_MM_INITIALIZE names a fd of the caller's.

use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

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
    pub const CREATE_RANGE_GROUP: u32 = 23;
    pub const DESTROY_RANGE_GROUP: u32 = 24;
    pub const REGISTER_GPU: u32 = 37;
    pub const UNREGISTER_GPU: u32 = 38;
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
/// `UVM_REGISTER_GPU_PARAMS`: `NvProcessorUuid gpu_uuid`, `NvBool
/// numaEnabled`, `NvS32 numaNodeId`, `NvS32 rmCtrlFd`, `NvHandle hClient`,
/// `NvHandle hSmcPartRef`, `NV_STATUS rmStatus`.
const REGISTER_GPU_SIZE: usize = 40;
const REGISTER_GPU_NUMA_ENABLED: usize = 16;
const REGISTER_GPU_NUMA_NODE_ID: usize = 20;
const REGISTER_GPU_STATUS: usize = 36;
/// `UVM_UNREGISTER_GPU_PARAMS`: `NvProcessorUuid gpu_uuid`, `NV_STATUS
/// rmStatus`.
const UNREGISTER_GPU_SIZE: usize = 20;
const UNREGISTER_GPU_STATUS: usize = 16;
/// `UVM_CREATE_RANGE_GROUP_PARAMS` and `UVM_DESTROY_RANGE_GROUP_PARAMS`:
/// `NvU64 rangeGroupId`, `NV_STATUS rmStatus`.
const RANGE_GROUP_SIZE: usize = 16;
const RANGE_GROUP_STATUS: usize = 8;

/// `UVM_INIT_FLAGS_MASK`: DISABLE_HMM and MULTI_PROCESS_SHARING_MODE.
const INIT_FLAGS_MASK: u64 = 0x3;

const NV_OK: u32 = 0;
const NV_ERR_GPU_UUID_NOT_FOUND: u32 = 0x12;
const NV_ERR_ILLEGAL_ACTION: u32 = 0x16;
const NV_ERR_INVALID_ARGUMENT: u32 = 0x1F;
const NV_ERR_INVALID_DEVICE: u32 = 0x26;
const NV_ERR_OBJECT_NOT_FOUND: u32 = 0x57;
const NV_WARN_NOTHING_TO_DO: u32 = 0x0001_0006;

/// A GPU's binary UUID (`NvProcessorUuid`).
type Uuid = [u8; nvctl::GPU_UUID_LEN];

/// The size of a served command's params and where their `rmStatus` is;
/// `None` for one not served.
fn layout(cmd: u32) -> Option<(usize, usize)> {
    Some(match cmd {
        cmd::INITIALIZE => (INITIALIZE_SIZE, INITIALIZE_STATUS),
        cmd::MM_INITIALIZE => (MM_INITIALIZE_SIZE, MM_INITIALIZE_STATUS),
        cmd::PAGEABLE_MEM_ACCESS => (PAGEABLE_MEM_ACCESS_SIZE, PAGEABLE_MEM_ACCESS_STATUS),
        cmd::REGISTER_GPU => (REGISTER_GPU_SIZE, REGISTER_GPU_STATUS),
        cmd::UNREGISTER_GPU => (UNREGISTER_GPU_SIZE, UNREGISTER_GPU_STATUS),
        cmd::CREATE_RANGE_GROUP | cmd::DESTROY_RANGE_GROUP => {
            (RANGE_GROUP_SIZE, RANGE_GROUP_STATUS)
        }
        _ => return None,
    })
}

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

/// What a VA space holds (`uvm_va_space_t`), of what is served: the GPUs
/// registered in it and its range groups. Only reached once the open is a
/// VA space.
#[derive(Debug, Default)]
struct VaSpace {
    gpus: BTreeSet<Uuid>,
    range_groups: BTreeSet<u64>,
    /// `range_group_id_counter`: the last id handed out. The first is 1, as
    /// 0 is UVM_RANGE_GROUP_ID_NONE, and none is handed out twice.
    last_range_group: u64,
}

/// One open of `/dev/nvidia-uvm`.
pub struct UvmOpen {
    inode_id: usize,
    fd_type: Mutex<FdType>,
    va_space: Mutex<VaSpace>,
}

impl UvmOpen {
    fn new(inode_id: usize) -> Self {
        UvmOpen {
            inode_id,
            fd_type: Mutex::new(FdType::Uninitialized),
            va_space: Mutex::new(VaSpace::default()),
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
        if cmd == cmd::DEINITIALIZE {
            return Ok(0);
        }
        let Some((size, _)) = layout(cmd) else {
            not_served(cmd);
            return Err(LxError::ENOSYS);
        };
        let mut params = UserInPtr::<u8>::from(arg)
            .read_array(size)
            .map_err(|_| LxError::EFAULT)?;
        self.call(cmd, &mut params, open_of_fd, &rm_gpus);
        UserOutPtr::<u8>::from(arg)
            .write_array(&params)
            .map_err(|_| LxError::EFAULT)?;
        Ok(0)
    }

    /// A served command on the kernel copy of its params: its answer goes in
    /// their `rmStatus`. `gpus` are the UUIDs of the GPUs UVM can register.
    fn call(
        &self,
        cmd: u32,
        params: &mut [u8],
        open_of_fd: &dyn Fn(i32) -> Option<Arc<dyn INode>>,
        gpus: &dyn Fn() -> Vec<Uuid>,
    ) {
        let status = match cmd {
            cmd::INITIALIZE => self.initialize(u64_at(params, 0)),
            cmd::MM_INITIALIZE => mm_initialize(i32_at(params, 0), open_of_fd),
            // The INIT_CHECK of the module's route for every other one: not
            // on an open that is not a VA space, whose params go back as
            // they came.
            _ if !self.is_va_space() => NV_ERR_ILLEGAL_ACTION,
            cmd::PAGEABLE_MEM_ACCESS => {
                params[0] = 0;
                NV_OK
            }
            cmd::REGISTER_GPU => self.register_gpu(params, gpus),
            cmd::UNREGISTER_GPU => self.unregister_gpu(&uuid_at(params, 0)),
            cmd::CREATE_RANGE_GROUP => self.create_range_group(params),
            cmd::DESTROY_RANGE_GROUP => self.destroy_range_group(u64_at(params, 0)),
            _ => unreachable!("only served commands get here"),
        };
        let (_, at) = layout(cmd).expect("only served commands get here");
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

    /// `uvm_api_register_gpu`: the GPU must be one the RM brought up
    /// (`uvm_gpu_retain_by_uuid`), and not registered here already. Its UUID
    /// goes back as it came (with no MIG, `gpu->uuid` is the one asked for),
    /// and it is not a NUMA node: its memory is not onlined as system memory
    /// here. rmCtrlFd, hClient and hSmcPartRef only matter to MIG.
    fn register_gpu(&self, params: &mut [u8], gpus: &dyn Fn() -> Vec<Uuid>) -> u32 {
        let uuid = uuid_at(params, 0);
        let known = gpus();
        if !known.contains(&uuid) {
            unknown_gpu(&uuid, known.len());
            return NV_ERR_GPU_UUID_NOT_FOUND;
        }
        if !self.va_space.lock().gpus.insert(uuid) {
            return NV_ERR_INVALID_DEVICE;
        }
        params[REGISTER_GPU_NUMA_ENABLED] = 0;
        params[REGISTER_GPU_NUMA_NODE_ID..REGISTER_GPU_NUMA_NODE_ID + 4]
            .copy_from_slice(&(-1i32).to_le_bytes());
        NV_OK
    }

    /// `uvm_api_unregister_gpu`: only a GPU registered in this VA space.
    fn unregister_gpu(&self, uuid: &Uuid) -> u32 {
        if self.va_space.lock().gpus.remove(uuid) {
            NV_OK
        } else {
            NV_ERR_INVALID_DEVICE
        }
    }

    /// `uvm_api_create_range_group`.
    fn create_range_group(&self, params: &mut [u8]) -> u32 {
        let mut va_space = self.va_space.lock();
        va_space.last_range_group += 1;
        let id = va_space.last_range_group;
        va_space.range_groups.insert(id);
        params[..8].copy_from_slice(&id.to_le_bytes());
        NV_OK
    }

    /// `uvm_api_destroy_range_group`: only a group of this VA space.
    fn destroy_range_group(&self, id: u64) -> u32 {
        if self.va_space.lock().range_groups.remove(&id) {
            NV_OK
        } else {
            NV_ERR_OBJECT_NOT_FOUND
        }
    }
}

/// The UUIDs of the GPUs the RM brought up, which are the ones UVM can
/// register (`nvidia_dev_get_uuid` finds no other).
fn rm_gpus() -> Vec<Uuid> {
    nvctl::cards()
        .iter()
        .filter_map(nvctl::Card::uuid)
        .collect()
}

/// Whether a refused UVM_REGISTER_GPU was logged already.
static UNKNOWN_GPU_LOGGED: AtomicBool = AtomicBool::new(false);

/// One log line the first time UVM_REGISTER_GPU names a GPU the RM does not
/// know: `cuInit` then answers 3, and this line is what says why.
fn unknown_gpu(uuid: &Uuid, known: usize) {
    if !UNKNOWN_GPU_LOGGED.swap(true, Ordering::Relaxed) {
        zcore_drivers::klog_warn!(
            "[nvidia-uvm] UVM_REGISTER_GPU: {} is none of the {} GPUs the RM brought up: NV_ERR_GPU_UUID_NOT_FOUND",
            ShowUuid(uuid),
            known
        );
    }
}

/// A UUID as `nvidia-smi -L` writes it.
struct ShowUuid<'a>(&'a Uuid);

impl fmt::Display for ShowUuid<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("GPU-")?;
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{:02x}", b)?;
        }
        Ok(())
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

fn uuid_at(b: &[u8], at: usize) -> Uuid {
    let mut v = [0u8; nvctl::GPU_UUID_LEN];
    v.copy_from_slice(&b[at..at + nvctl::GPU_UUID_LEN]);
    v
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
        "[nvidia-uvm] /dev/nvidia-uvm: cuInit's calls served, the rest logged"
    );
}

#[cfg(test)]
mod tests;
