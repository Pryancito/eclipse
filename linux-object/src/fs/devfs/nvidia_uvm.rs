//! `/dev/nvidia-uvm` (hatch `nvidia.uvm`, default off, and only beside
//! `/dev/nvidiactl`): the device file of NVIDIA's unified-memory driver, the
//! second one CUDA opens.
//!
//! On Linux it is a module of its own, kernel-open/nvidia-uvm. libcuda's
//! `cuInit` opens it twice and makes three calls; if the file is missing, or
//! any of the three fails, `cuInit` answers CUDA_ERROR_UNKNOWN (999) and no
//! CUDA program gets a device. Once it has the devices it registers each
//! GPU with UVM and makes a range group, and if either fails `cuInit`
//! answers CUDA_ERROR_NOT_INITIALIZED (3). `cuCtxCreate` then hands UVM the
//! GPU VA space of the context, maps the memory the context needs into it,
//! registers the context's channels and makes a semaphore pool, which it
//! maps for the CPU and then asks UVM to validate; if UVM refuses the GPU VA
//! space, the pool or its validation, `cuCtxCreate` answers
//! CUDA_ERROR_NOT_SUPPORTED (801). All of them are served here as uvm.c
//! serves them:
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
//! - UVM_REGISTER_GPU_VASPACE takes a client's FERMI_VASPACE_A as the VA
//!   space of a registered GPU, and UVM_UNREGISTER_GPU_VASPACE gives it back
//!   with everything mapped in it.
//! - UVM_CREATE_EXTERNAL_RANGE reserves a range of GPU virtual addresses
//!   that UVM_MAP_EXTERNAL_ALLOCATION maps RM memory into, on each GPU it
//!   names; UVM_UNMAP_EXTERNAL unmaps it from one GPU, and UVM_FREE gives
//!   the range back.
//! - UVM_REGISTER_CHANNEL and UVM_UNREGISTER_CHANNEL keep the list of a GPU
//!   VA space's channels.
//! - UVM_ALLOC_SEMAPHORE_POOL makes a range of zeroed system memory that
//!   each GPU with a GPU VA space maps at its address, and an mmap of
//!   exactly that range maps for the CPU; UVM_FREE gives it back once the
//!   CPU has unmapped it.
//! - UVM_VALIDATE_VA_RANGE answers whether a range is exactly one here,
//!   external or a semaphore pool. Linux also counts the ranges it makes for
//!   a channel's buffers, which this port leaves to the RM (below), so one
//!   of those is not a range here.
//!
//! One departure, the whole design of GPU VA spaces and of what is mapped in
//! them: on Linux UVM owns the page tables of a GPU VA space registered with
//! it (libcuda allocates it IS_EXTERNALLY_OWNED, so the RM never writes a PTE
//! there) and writes every PTE of its mappings itself. This port has no UVM
//! page-table code, so the RM keeps them. An RM_ALLOC of such a VA space goes
//! to the RM without IS_EXTERNALLY_OWNED and ENABLE_PAGE_FAULTING
//! ([`rm_owned_va_space`]), each UVM mapping becomes an RM mapping in it,
//! made as the user through the `/dev/nvidiactl` open that made the client
//! (an NV50_MEMORY_VIRTUAL at the address UVM was given, and the memory
//! mapped into it), and a channel needs nothing from UVM, as the RM maps a
//! channel's own buffers in a VA space it owns. A semaphore pool's pages are
//! the kernel's, made RM memory of the GPU VA space's client (an
//! NV01_MEMORY_SYSTEM_OS_DESCRIPTOR of their physical address) and mapped
//! like an external allocation. Without page faulting a GPU access outside
//! every mapping is fatal to its channel instead of a fault UVM serves, which
//! nothing served here relies on.
//!
//! UVM_DEINITIALIZE succeeds and does nothing, as there. Every other command
//! is ENOSYS, the module's answer to one it does not know, and is logged by
//! name the first time it is asked for: that line says what the next batch
//! needs. A served call that fails is logged too, with its status, the
//! first time each command fails with each status.
//!
//! A command is a plain number (`UVM_IOCTL_BASE(i)` is `i` on Linux), not an
//! `_IOC` encoding. A served one answers through its params' `rmStatus`, and
//! the ioctl itself fails only when the params cannot be copied. It arrives
//! from `sys_ioctl` (linux-syscall), like the escapes of `/dev/nvidiactl`,
//! because UVM_MM_INITIALIZE and UVM_REGISTER_GPU_VASPACE name a fd of the
//! caller's.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use kernel_hal::user::{UserInPtr, UserOutPtr};
use lock::Mutex;
use rcore_fs::vfs::*;
use rcore_fs_devfs::{DevFS, DevINode};
use zcore_drivers::display::nvctl;
use zircon_object::vm::{MMUFlags, VmObject};

use super::nvidia::{NvCaller, NvidiaOpen};
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
    pub const REGISTER_GPU_VASPACE: u32 = 25;
    pub const UNREGISTER_GPU_VASPACE: u32 = 26;
    pub const REGISTER_CHANNEL: u32 = 27;
    pub const UNREGISTER_CHANNEL: u32 = 28;
    pub const MAP_EXTERNAL_ALLOCATION: u32 = 33;
    pub const FREE: u32 = 34;
    pub const REGISTER_GPU: u32 = 37;
    pub const UNREGISTER_GPU: u32 = 38;
    pub const PAGEABLE_MEM_ACCESS: u32 = 39;
    pub const UNMAP_EXTERNAL: u32 = 66;
    pub const ALLOC_SEMAPHORE_POOL: u32 = 68;
    pub const VALIDATE_VA_RANGE: u32 = 72;
    pub const CREATE_EXTERNAL_RANGE: u32 = 73;
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

/// `UVM_REGISTER_GPU_VASPACE_PARAMS`: `NvProcessorUuid gpuUuid`, `NvS32
/// rmCtrlFd`, `NvHandle hClient`, `NvHandle hVaSpace`, `NV_STATUS rmStatus`.
const REGISTER_GPU_VASPACE_SIZE: usize = 32;
const REGISTER_GPU_VASPACE_CTRL_FD: usize = 16;
const REGISTER_GPU_VASPACE_CLIENT: usize = 20;
const REGISTER_GPU_VASPACE_VA_SPACE: usize = 24;
const REGISTER_GPU_VASPACE_STATUS: usize = 28;
/// `UVM_UNREGISTER_GPU_VASPACE_PARAMS`: `NvProcessorUuid gpuUuid`,
/// `NV_STATUS rmStatus`.
const UNREGISTER_GPU_VASPACE_SIZE: usize = 20;
const UNREGISTER_GPU_VASPACE_STATUS: usize = 16;
/// `UVM_REGISTER_CHANNEL_PARAMS`: `NvProcessorUuid gpuUuid`, `NvS32
/// rmCtrlFd`, `NvHandle hClient`, `NvHandle hChannel`, `NvU64 base`,
/// `NvU64 length`, `NV_STATUS rmStatus`.
const REGISTER_CHANNEL_SIZE: usize = 56;
const REGISTER_CHANNEL_CLIENT: usize = 20;
const REGISTER_CHANNEL_CHANNEL: usize = 24;
const REGISTER_CHANNEL_STATUS: usize = 48;
/// `UVM_UNREGISTER_CHANNEL_PARAMS`: `NvProcessorUuid gpuUuid`, `NvHandle
/// hClient`, `NvHandle hChannel`, `NV_STATUS rmStatus`.
const UNREGISTER_CHANNEL_SIZE: usize = 28;
const UNREGISTER_CHANNEL_CLIENT: usize = 16;
const UNREGISTER_CHANNEL_CHANNEL: usize = 20;
const UNREGISTER_CHANNEL_STATUS: usize = 24;
/// `UVM_MAP_EXTERNAL_ALLOCATION_PARAMS`: `NvU64 base`, `NvU64 length`,
/// `NvU64 offset`, `UvmGpuMappingAttributes
/// perGpuAttributes[UVM_MAX_GPUS]`, `NvU64 gpuAttributesCount`, `NvS32
/// rmCtrlFd`, `NvHandle hClient`, `NvHandle hMemory`, `NV_STATUS rmStatus`.
const MAP_SIZE: usize = 9264;
const MAP_OFFSET: usize = 16;
const MAP_ATTRIBUTES: usize = 24;
const MAP_COUNT: usize = 9240;
const MAP_CLIENT: usize = 9252;
const MAP_MEMORY: usize = 9256;
const MAP_STATUS: usize = 9260;
/// `UvmGpuMappingAttributes`: `NvProcessorUuid gpuUuid`, then
/// `gpuMappingType`, `gpuCachingType`, `gpuFormatType`, `gpuElementBits`
/// and `gpuCompressionType`, an `NvU32` each.
const ATTRIBUTES_SIZE: usize = 36;
/// `UVM_MAX_GPUS`: the entries of `perGpuAttributes`.
const MAX_GPUS: u64 = 256;
/// `UVM_FREE_PARAMS`, `UVM_VALIDATE_VA_RANGE_PARAMS` and
/// `UVM_CREATE_EXTERNAL_RANGE_PARAMS`: `NvU64 base`, `NvU64 length`,
/// `NV_STATUS rmStatus`.
const RANGE_SIZE: usize = 24;
const RANGE_STATUS: usize = 16;
/// `UVM_UNMAP_EXTERNAL_PARAMS`: `NvU64 base`, `NvU64 length`,
/// `NvProcessorUuid gpuUuid`, `NV_STATUS rmStatus`.
const UNMAP_EXTERNAL_SIZE: usize = 40;
const UNMAP_EXTERNAL_GPU: usize = 16;
const UNMAP_EXTERNAL_STATUS: usize = 32;
/// `UVM_ALLOC_SEMAPHORE_POOL_PARAMS`: `NvU64 base`, `NvU64 length`,
/// `UvmGpuMappingAttributes perGpuAttributes[UVM_MAX_GPUS]`, `NvU64
/// gpuAttributesCount`, `NV_STATUS rmStatus`.
const POOL_SIZE: usize = 9248;
const POOL_ATTRIBUTES: usize = 16;
const POOL_COUNT: usize = 9232;
const POOL_STATUS: usize = 9240;

/// `UVM_INIT_FLAGS_MASK`: DISABLE_HMM and MULTI_PROCESS_SHARING_MODE.
const INIT_FLAGS_MASK: u64 = 0x3;

/// `UVM_PAGE_SIZE_4K`, the alignment every range is checked for.
const PAGE_4K: u64 = 0x1000;

const NV_OK: u32 = 0;
const NV_ERR_GPU_UUID_NOT_FOUND: u32 = 0x12;
const NV_ERR_ILLEGAL_ACTION: u32 = 0x16;
const NV_ERR_INVALID_ADDRESS: u32 = 0x1E;
const NV_ERR_INVALID_ARGUMENT: u32 = 0x1F;
const NV_ERR_INVALID_CHANNEL: u32 = 0x21;
const NV_ERR_INVALID_DEVICE: u32 = 0x26;
const NV_ERR_NO_MEMORY: u32 = 0x51;
const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
const NV_ERR_OBJECT_NOT_FOUND: u32 = 0x57;
const NV_ERR_UVM_ADDRESS_IN_USE: u32 = 0x68;
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
        cmd::REGISTER_GPU_VASPACE => (REGISTER_GPU_VASPACE_SIZE, REGISTER_GPU_VASPACE_STATUS),
        cmd::UNREGISTER_GPU_VASPACE => (UNREGISTER_GPU_VASPACE_SIZE, UNREGISTER_GPU_VASPACE_STATUS),
        cmd::REGISTER_CHANNEL => (REGISTER_CHANNEL_SIZE, REGISTER_CHANNEL_STATUS),
        cmd::UNREGISTER_CHANNEL => (UNREGISTER_CHANNEL_SIZE, UNREGISTER_CHANNEL_STATUS),
        cmd::MAP_EXTERNAL_ALLOCATION => (MAP_SIZE, MAP_STATUS),
        cmd::FREE | cmd::VALIDATE_VA_RANGE | cmd::CREATE_EXTERNAL_RANGE => {
            (RANGE_SIZE, RANGE_STATUS)
        }
        cmd::UNMAP_EXTERNAL => (UNMAP_EXTERNAL_SIZE, UNMAP_EXTERNAL_STATUS),
        cmd::ALLOC_SEMAPHORE_POOL => (POOL_SIZE, POOL_STATUS),
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

/// What a VA space holds (`uvm_va_space_t`), of what is served. Only
/// reached once the open is a VA space.
#[derive(Default)]
struct VaSpace {
    gpus: BTreeSet<Uuid>,
    range_groups: BTreeSet<u64>,
    /// `range_group_id_counter`: the last id handed out. The first is 1, as
    /// 0 is UVM_RANGE_GROUP_ID_NONE, and none is handed out twice.
    last_range_group: u64,
    /// The GPU VA spaces, by GPU: at most one per registered GPU.
    gpu_va_spaces: BTreeMap<Uuid, GpuVaSpace>,
    /// The external ranges, by first byte.
    ranges: BTreeMap<u64, ExternalRange>,
    /// The semaphore pools, by first byte. No two ranges overlap, of either
    /// kind ([`VaSpace::overlaps`]).
    pools: BTreeMap<u64, SemaphorePool>,
}

impl VaSpace {
    /// Whether `[base, end)` overlaps a range here, external or a pool: the
    /// collision `uvm_range_tree_add` refuses.
    fn overlaps(&self, base: u64, end: u64) -> bool {
        let range = self
            .ranges
            .range(..end)
            .next_back()
            .is_some_and(|(_, range)| range.end > base);
        let pool = self
            .pools
            .range(..end)
            .next_back()
            .is_some_and(|(_, pool)| pool.end > base);
        range || pool
    }

    /// `uvm_va_range_find`: the range here, external or a pool, that holds
    /// `addr`, as its first byte and one past its last.
    fn range_at(&self, addr: u64) -> Option<(u64, u64)> {
        let range = self
            .ranges
            .range(..=addr)
            .next_back()
            .map(|(&base, range)| (base, range.end));
        let pool = self
            .pools
            .range(..=addr)
            .next_back()
            .map(|(&base, pool)| (base, pool.end));
        range.into_iter().chain(pool).find(|&(_, end)| end > addr)
    }
}

/// A GPU VA space (`uvm_gpu_va_space_t`): the client's FERMI_VASPACE_A, and
/// the open its RM calls are made through.
struct GpuVaSpace {
    ctl: CtlOpen,
    h_client: u32,
    /// The device the VA space hangs from, which its mappings are made
    /// under.
    h_device: u32,
    h_va_space: u32,
    /// The channels registered on it, as (hClient, hChannel).
    channels: BTreeSet<(u32, u32)>,
}

/// An external range (`uvm_va_range_external_t`).
struct ExternalRange {
    /// One past its last byte.
    end: u64,
    /// What is mapped in it, by GPU and then by first byte; on one GPU the
    /// mappings never overlap. A GPU has an entry only while it has a GPU
    /// VA space, whose client every one of them was made in.
    maps: BTreeMap<Uuid, BTreeMap<u64, Mapping>>,
}

/// A semaphore pool (`uvm_va_range_semaphore_pool_t`): system memory the
/// kernel allocates, which the GPUs and the CPU all map at its first byte.
struct SemaphorePool {
    /// One past its last byte.
    end: u64,
    /// Its pages: zeroed, contiguous and pinned. An mmap of the pool maps
    /// this very object.
    memory: Arc<VmObject>,
    /// The physical address of its first page.
    phys: u64,
    /// What keeps `memory` allocated, which the RM holds a clone of for as
    /// long as it has RM memory made of the pages ([`nvctl::Pages`]).
    keep: Arc<dyn Any + Send + Sync>,
    /// How each GPU `perGpuAttributes` named maps it; any other GPU maps it
    /// as [`PoolAttrs::default`] says.
    attrs: BTreeMap<Uuid, PoolAttrs>,
    /// Its mapping on each GPU with a GPU VA space.
    maps: BTreeMap<Uuid, PoolMapping>,
}

impl SemaphorePool {
    /// `uvm_mem_mapped_on_cpu_user`: whether anything holds the pages but
    /// the pool and what it keeps them with, which is a CPU mapping of them.
    fn mapped_on_cpu(&self) -> bool {
        Arc::strong_count(&self.memory) > 2
    }
}

/// How a GPU maps a semaphore pool (`uvm_mem_gpu_mapping_attrs_t`); the
/// default is read-write and not cached by the GPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PoolAttrs {
    read_only: bool,
    cached: bool,
}

/// A semaphore pool's mapping on one GPU, both halves made in the GPU VA
/// space's client: the pages as RM memory, and the NV50_MEMORY_VIRTUAL at
/// the pool's address they are mapped into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PoolMapping {
    h_memory: u32,
    h_virtual: u32,
}

/// One GPU's mapping in an external range (`uvm_ext_gpu_map_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mapping {
    /// One past its last byte.
    end: u64,
    /// The NV50_MEMORY_VIRTUAL that holds it, in the GPU VA space's client:
    /// freeing it unmaps it.
    h_virtual: u32,
}

/// The `/dev/nvidiactl` open a GPU VA space's client was made through,
/// which the RM checks that client against on every call made on it.
#[derive(Clone)]
struct CtlOpen {
    /// Not kept alive: closing it frees its clients, with every object
    /// made in them, as on Linux, and then there is nothing left to free.
    open: Weak<dyn INode>,
    os_info: usize,
}

impl CtlOpen {
    /// The open behind the caller's fd `fd`, if it is one of
    /// `/dev/nvidiactl`.
    fn of_fd(fd: i32, open_of_fd: &dyn Fn(i32) -> Option<Arc<dyn INode>>) -> Option<CtlOpen> {
        let inode = open_of_fd(fd)?;
        let os_info = inode.downcast_ref::<NvidiaOpen>()?.uvm_os_info()?;
        Some(CtlOpen {
            open: Arc::downgrade(&inode),
            os_info,
        })
    }

    /// Who an RM call is made as, with the open held for the length of the
    /// call so its `os_info` names it throughout; `None` once it is closed.
    fn caller(&self, who: Who) -> Option<(Arc<dyn INode>, nvctl::Caller)> {
        let open = self.open.upgrade()?;
        let caller = nvctl::Caller {
            os_info: self.os_info,
            ctl: true,
            admin: who.admin,
            gpu_mask: who.gpu_mask,
        };
        Some((open, caller))
    }
}

/// Whom a UVM call is for, which the RM calls it makes are made with.
#[derive(Clone, Copy, Debug)]
struct Who {
    admin: bool,
    /// The RM GPU instances the caller has open.
    gpu_mask: u32,
}

impl Who {
    fn of(caller: &NvCaller) -> Who {
        Who {
            admin: caller.admin,
            gpu_mask: (caller.gpu_mask)(),
        }
    }

    /// The close, which has no caller to ask about, as `release` frees an
    /// open's clients: nobody is an administrator and no GPU is open.
    const NOBODY: Who = Who {
        admin: false,
        gpu_mask: 0,
    };
}

/// An RM handle, or the RM's status refusing to make one.
type RmResult = core::result::Result<u32, u32>;

/// What UVM asks of the RM: the kernel's ([`KernelRm`]), or a test's
/// stand-in.
trait Rm: Send + Sync {
    /// The GPUs the RM brought up, each with its RM instance: the ones UVM
    /// can register (`nvidia_dev_get_uuid` finds no other).
    fn gpus(&self) -> Vec<(Uuid, u32)>;
    /// `nvctl::uvm_va_space_device`.
    fn va_space_device(
        &self,
        by: &nvctl::Caller,
        h_client: u32,
        h_va_space: u32,
        instance: u32,
    ) -> RmResult;
    /// `nvctl::uvm_map_external`.
    fn map(&self, by: &nvctl::Caller, m: &nvctl::ExternalMapping) -> RmResult;
    /// `nvctl::uvm_pages`.
    fn pages(
        &self,
        by: &nvctl::Caller,
        h_client: u32,
        h_device: u32,
        pages: &nvctl::Pages,
        gpu_cached: bool,
    ) -> RmResult;
    /// `nvctl::uvm_free`.
    fn free(&self, by: &nvctl::Caller, h_client: u32, h_object: u32);
}

struct KernelRm;

impl Rm for KernelRm {
    fn gpus(&self) -> Vec<(Uuid, u32)> {
        nvctl::cards()
            .iter()
            .filter_map(|card| Some((card.uuid()?, card.instance?)))
            .collect()
    }

    fn va_space_device(
        &self,
        by: &nvctl::Caller,
        h_client: u32,
        h_va_space: u32,
        instance: u32,
    ) -> RmResult {
        nvctl::uvm_va_space_device(by, h_client, h_va_space, instance)
    }

    fn map(&self, by: &nvctl::Caller, m: &nvctl::ExternalMapping) -> RmResult {
        nvctl::uvm_map_external(by, m)
    }

    fn pages(
        &self,
        by: &nvctl::Caller,
        h_client: u32,
        h_device: u32,
        pages: &nvctl::Pages,
        gpu_cached: bool,
    ) -> RmResult {
        nvctl::uvm_pages(by, h_client, h_device, pages, gpu_cached)
    }

    fn free(&self, by: &nvctl::Caller, h_client: u32, h_object: u32) {
        let status = nvctl::uvm_free(by, h_client, h_object);
        if status != NV_OK {
            unmap_refused(status);
        }
    }
}

/// One open of `/dev/nvidia-uvm`.
pub struct UvmOpen {
    inode_id: usize,
    fd_type: Mutex<FdType>,
    /// Held across the RM calls a command makes, which run with interrupts
    /// on (the RM gate's own discipline): a `lock::Mutex` would turn them off
    /// for the whole call.
    va_space: spin::Mutex<VaSpace>,
    rm: Arc<dyn Rm>,
}

impl UvmOpen {
    fn new(inode_id: usize) -> Self {
        UvmOpen::with_rm(inode_id, Arc::new(KernelRm))
    }

    fn with_rm(inode_id: usize, rm: Arc<dyn Rm>) -> Self {
        UvmOpen {
            inode_id,
            fd_type: Mutex::new(FdType::Uninitialized),
            va_space: spin::Mutex::new(VaSpace::default()),
            rm,
        }
    }

    /// `uvm_fd_va_space`: whether UVM_INITIALIZE made this open a VA space.
    fn is_va_space(&self) -> bool {
        matches!(*self.fd_type.lock(), FdType::VaSpace { .. })
    }

    /// `uvm_ioctl`. `caller.open_of_fd` is the inode behind one of the
    /// caller's fds, for UVM_MM_INITIALIZE and UVM_REGISTER_GPU_VASPACE.
    pub fn ioctl(&self, cmd: u32, arg: usize, caller: &NvCaller) -> LxResult<usize> {
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
        self.call(cmd, &mut params, caller);
        UserOutPtr::<u8>::from(arg)
            .write_array(&params)
            .map_err(|_| LxError::EFAULT)?;
        Ok(0)
    }

    /// A served command on the kernel copy of its params: its answer goes in
    /// their `rmStatus`.
    fn call(&self, cmd: u32, params: &mut [u8], caller: &NvCaller) {
        let status = match cmd {
            cmd::INITIALIZE => self.initialize(u64_at(params, 0)),
            cmd::MM_INITIALIZE => mm_initialize(i32_at(params, 0), caller.open_of_fd),
            // The INIT_CHECK of the module's route for every other one: not
            // on an open that is not a VA space, whose params go back as
            // they came.
            _ if !self.is_va_space() => NV_ERR_ILLEGAL_ACTION,
            cmd::PAGEABLE_MEM_ACCESS => {
                params[0] = 0;
                NV_OK
            }
            cmd::REGISTER_GPU => self.register_gpu(params),
            cmd::UNREGISTER_GPU => self.unregister_gpu(&uuid_at(params, 0), Who::of(caller)),
            cmd::CREATE_RANGE_GROUP => self.create_range_group(params),
            cmd::DESTROY_RANGE_GROUP => self.destroy_range_group(u64_at(params, 0)),
            cmd::REGISTER_GPU_VASPACE => self.register_gpu_va_space(params, caller),
            cmd::UNREGISTER_GPU_VASPACE => {
                self.unregister_gpu_va_space(&uuid_at(params, 0), Who::of(caller))
            }
            cmd::CREATE_EXTERNAL_RANGE => {
                self.create_external_range(u64_at(params, 0), u64_at(params, 8))
            }
            cmd::MAP_EXTERNAL_ALLOCATION => self.map_external(params, Who::of(caller)),
            cmd::UNMAP_EXTERNAL => self.unmap_external(params, Who::of(caller)),
            cmd::FREE => self.free(u64_at(params, 0), u64_at(params, 8), Who::of(caller)),
            cmd::ALLOC_SEMAPHORE_POOL => self.alloc_semaphore_pool(params, Who::of(caller)),
            cmd::VALIDATE_VA_RANGE => self.validate_va_range(u64_at(params, 0), u64_at(params, 8)),
            cmd::REGISTER_CHANNEL => self.register_channel(params),
            cmd::UNREGISTER_CHANNEL => self.unregister_channel(
                u32_at(params, UNREGISTER_CHANNEL_CLIENT),
                u32_at(params, UNREGISTER_CHANNEL_CHANNEL),
            ),
            _ => unreachable!("only served commands get here"),
        };
        if status != NV_OK && status != NV_WARN_NOTHING_TO_DO {
            failed(cmd, status);
        }
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
    fn register_gpu(&self, params: &mut [u8]) -> u32 {
        let uuid = uuid_at(params, 0);
        let known = self.rm.gpus();
        if !known.iter().any(|&(gpu, _)| gpu == uuid) {
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

    /// `uvm_api_unregister_gpu`: only a GPU registered in this VA space, and
    /// its GPU VA space goes with it.
    fn unregister_gpu(&self, uuid: &Uuid, who: Who) -> u32 {
        let mut va_space = self.va_space.lock();
        if !va_space.gpus.remove(uuid) {
            return NV_ERR_INVALID_DEVICE;
        }
        self.remove_gpu_va_space(&mut va_space, uuid, who);
        NV_OK
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

    /// `uvm_va_space_register_gpu_va_space`: the GPU must be registered here
    /// and have no GPU VA space yet, and the RM must find `hVaSpace` a
    /// FERMI_VASPACE_A of `hClient` on that GPU, `hClient` a client of the
    /// `/dev/nvidiactl` open behind `rmCtrlFd`. Linux does not look at the
    /// fd, as its UVM reaches the client through a client of its own; here
    /// it is what the RM calls on the client are made through.
    fn register_gpu_va_space(&self, params: &[u8], caller: &NvCaller) -> u32 {
        let uuid = uuid_at(params, 0);
        let mut va_space = self.va_space.lock();
        if !va_space.gpus.contains(&uuid) || va_space.gpu_va_spaces.contains_key(&uuid) {
            return NV_ERR_INVALID_DEVICE;
        }
        let Some(instance) = self
            .rm
            .gpus()
            .iter()
            .find(|&&(gpu, _)| gpu == uuid)
            .map(|&(_, instance)| instance)
        else {
            return NV_ERR_INVALID_DEVICE;
        };
        let fd = i32_at(params, REGISTER_GPU_VASPACE_CTRL_FD);
        let Some(ctl) = CtlOpen::of_fd(fd, caller.open_of_fd) else {
            not_a_ctl_open(fd);
            return NV_ERR_INVALID_ARGUMENT;
        };
        let Some((_open, by)) = ctl.caller(Who::of(caller)) else {
            return NV_ERR_INVALID_ARGUMENT;
        };
        let h_client = u32_at(params, REGISTER_GPU_VASPACE_CLIENT);
        let h_va_space = u32_at(params, REGISTER_GPU_VASPACE_VA_SPACE);
        let h_device = match self.rm.va_space_device(&by, h_client, h_va_space, instance) {
            Ok(h_device) => h_device,
            Err(status) => return status,
        };
        va_space.gpu_va_spaces.insert(
            uuid,
            GpuVaSpace {
                ctl,
                h_client,
                h_device,
                h_va_space,
                channels: BTreeSet::new(),
            },
        );
        // `uvm_va_range_add_gpu_va_space` of each semaphore pool: one that
        // cannot be mapped on it fails the registration, and what was mapped
        // goes with the GPU VA space.
        if let Err(status) = self.map_pools_on(&mut va_space, &uuid, Who::of(caller)) {
            self.remove_gpu_va_space(&mut va_space, &uuid, Who::of(caller));
            return status;
        }
        first_time(
            &FIRST_GPU_VA_SPACE,
            "UVM_REGISTER_GPU_VASPACE: the first GPU VA space registered; its page tables stay the RM's",
        );
        NV_OK
    }

    /// `uvm_va_space_unregister_gpu_va_space`: only a GPU with a GPU VA
    /// space here.
    fn unregister_gpu_va_space(&self, uuid: &Uuid, who: Who) -> u32 {
        let mut va_space = self.va_space.lock();
        if self.remove_gpu_va_space(&mut va_space, uuid, who) {
            NV_OK
        } else {
            NV_ERR_INVALID_DEVICE
        }
    }

    /// `remove_gpu_va_space`: the GPU VA space of `uuid`, if it has one, with
    /// its channels and every mapping on that GPU, of external ranges and of
    /// semaphore pools.
    fn remove_gpu_va_space(&self, va_space: &mut VaSpace, uuid: &Uuid, who: Who) -> bool {
        let Some(gpu_va_space) = va_space.gpu_va_spaces.remove(uuid) else {
            return false;
        };
        for range in va_space.ranges.values_mut() {
            if let Some(maps) = range.maps.remove(uuid) {
                self.unmap(&gpu_va_space, maps.into_values(), who);
            }
        }
        for pool in va_space.pools.values_mut() {
            if let Some(map) = pool.maps.remove(uuid) {
                self.unmap_pool(&gpu_va_space, map, who);
            }
        }
        true
    }

    /// Every semaphore pool here mapped on the GPU VA space of `uuid`, which
    /// has none of them mapped yet. The first that fails stops it, with its
    /// status.
    fn map_pools_on(
        &self,
        va_space: &mut VaSpace,
        uuid: &Uuid,
        who: Who,
    ) -> core::result::Result<(), u32> {
        let gpu_va_space = &va_space.gpu_va_spaces[uuid];
        for (&base, pool) in va_space.pools.iter_mut() {
            self.map_pool(gpu_va_space, uuid, base, pool, who)?;
        }
        Ok(())
    }

    /// `va_range_add_gpu_va_space_semaphore_pool`: the pool at `base` mapped
    /// at `base` on the GPU VA space of `uuid`, as the user. Its pages are
    /// lent to the RM as memory of the GPU VA space's client, which is then
    /// mapped like an external allocation, and freed again if that fails.
    fn map_pool(
        &self,
        gpu_va_space: &GpuVaSpace,
        uuid: &Uuid,
        base: u64,
        pool: &mut SemaphorePool,
        who: Who,
    ) -> core::result::Result<(), u32> {
        let Some((_open, by)) = gpu_va_space.ctl.caller(who) else {
            return Err(nvctl::NV_ERR_INVALID_CLIENT);
        };
        let attrs = pool.attrs.get(uuid).copied().unwrap_or_default();
        let pages = nvctl::Pages {
            phys: pool.phys,
            size: pool.end - base,
            keep: &pool.keep,
        };
        let h_client = gpu_va_space.h_client;
        let h_memory = self
            .rm
            .pages(&by, h_client, gpu_va_space.h_device, &pages, attrs.cached)?;
        let mapping = nvctl::ExternalMapping {
            h_client,
            h_device: gpu_va_space.h_device,
            h_va_space: gpu_va_space.h_va_space,
            h_memory,
            offset: 0,
            base,
            length: pool.end - base,
            read_only: attrs.read_only,
        };
        match self.rm.map(&by, &mapping) {
            Ok(h_virtual) => {
                pool.maps.insert(
                    *uuid,
                    PoolMapping {
                        h_memory,
                        h_virtual,
                    },
                );
                Ok(())
            }
            Err(status) => {
                self.rm.free(&by, h_client, h_memory);
                Err(status)
            }
        }
    }

    /// A semaphore pool's mapping on one GPU undone, as the user: the
    /// virtual object freed, then the memory, unless the open of their
    /// client is closed and both went with it. The RM gives the pages back
    /// once nothing it has holds them.
    fn unmap_pool(&self, gpu_va_space: &GpuVaSpace, map: PoolMapping, who: Who) {
        let Some((_open, by)) = gpu_va_space.ctl.caller(who) else {
            return;
        };
        self.rm.free(&by, gpu_va_space.h_client, map.h_virtual);
        self.rm.free(&by, gpu_va_space.h_client, map.h_memory);
    }

    /// Every mapping of `pool`, on every GPU.
    fn unmap_pool_everywhere(
        &self,
        gpu_va_spaces: &BTreeMap<Uuid, GpuVaSpace>,
        pool: SemaphorePool,
        who: Who,
    ) {
        for (uuid, map) in pool.maps {
            // A GPU has a mapping only while it has a GPU VA space.
            if let Some(gpu_va_space) = gpu_va_spaces.get(&uuid) {
                self.unmap_pool(gpu_va_space, map, who);
            }
        }
    }

    /// The RM's half of an unmap: each mapping's virtual object freed, as
    /// the user, unless the open of its client is closed and they all went
    /// with it.
    fn unmap(&self, gpu_va_space: &GpuVaSpace, maps: impl Iterator<Item = Mapping>, who: Who) {
        let Some((_open, by)) = gpu_va_space.ctl.caller(who) else {
            return;
        };
        for map in maps {
            self.rm.free(&by, gpu_va_space.h_client, map.h_virtual);
        }
    }

    /// `uvm_create_external_range`: a range of whole 4 KiB pages, not
    /// overlapping one already here.
    fn create_external_range(&self, base: u64, length: u64) -> u32 {
        let Some(end) = range_end(base, length) else {
            return NV_ERR_INVALID_ADDRESS;
        };
        let mut va_space = self.va_space.lock();
        if va_space.overlaps(base, end) {
            return NV_ERR_UVM_ADDRESS_IN_USE;
        }
        va_space.ranges.insert(
            base,
            ExternalRange {
                end,
                maps: BTreeMap::new(),
            },
        );
        NV_OK
    }

    /// `uvm_free`: only a whole range, external or a semaphore pool, with
    /// what is mapped of it. A pool must be unmapped from the CPU first.
    fn free(&self, base: u64, length: u64, who: Who) -> u32 {
        let Some(end) = range_end(base, length) else {
            return NV_ERR_INVALID_ADDRESS;
        };
        let mut va_space = self.va_space.lock();
        if va_space.pools.get(&base).map(|pool| pool.end) == Some(end) {
            if va_space.pools[&base].mapped_on_cpu() {
                return NV_ERR_INVALID_ARGUMENT;
            }
            let pool = va_space.pools.remove(&base).expect("just found");
            self.unmap_pool_everywhere(&va_space.gpu_va_spaces, pool, who);
            return NV_OK;
        }
        if va_space.ranges.get(&base).map(|range| range.end) != Some(end) {
            return NV_ERR_INVALID_ADDRESS;
        }
        let range = va_space.ranges.remove(&base).expect("just found");
        for (uuid, maps) in range.maps {
            // A GPU has mappings only while it has a GPU VA space.
            if let Some(gpu_va_space) = va_space.gpu_va_spaces.get(&uuid) {
                self.unmap(gpu_va_space, maps.into_values(), who);
            }
        }
        NV_OK
    }

    /// `uvm_map_external_allocation`, one GPU after another: `length` bytes
    /// of the memory from `offset` at `base`, which must lie in one external
    /// range, on each GPU named, which must have a GPU VA space. What was
    /// mapped there on that GPU is unmapped first, and when a GPU fails,
    /// what this call mapped is unmapped again.
    ///
    /// The memory must be of the GPU VA space's own client: the mapping is
    /// made in that client, where Linux UVM dups the memory into its own.
    /// A caching type, format or compression other than the default is
    /// accepted and left to the RM, which takes them from the memory.
    fn map_external(&self, params: &[u8], who: Who) -> u32 {
        let base = u64_at(params, 0);
        let length = u64_at(params, 8);
        let Some(end) = range_end(base, length) else {
            return NV_ERR_INVALID_ADDRESS;
        };
        let count = u64_at(params, MAP_COUNT);
        if count == 0 || count > MAX_GPUS {
            return NV_ERR_INVALID_ARGUMENT;
        }
        let mut va_space = self.va_space.lock();
        let Some(range_base) = range_holding(&va_space.ranges, base, end) else {
            return NV_ERR_INVALID_ADDRESS;
        };
        let h_client = u32_at(params, MAP_CLIENT);
        let mut mapped = Vec::new();
        let mut status = NV_OK;
        for i in 0..count as usize {
            let attributes = &params[MAP_ATTRIBUTES + i * ATTRIBUTES_SIZE..][..ATTRIBUTES_SIZE];
            status = self.map_on_gpu(
                &mut va_space,
                range_base,
                attributes,
                h_client,
                u32_at(params, MAP_MEMORY),
                u64_at(params, MAP_OFFSET),
                (base, end),
                who,
            );
            if status != NV_OK {
                break;
            }
            mapped.push(uuid_at(attributes, 0));
        }
        if status != NV_OK {
            for uuid in mapped {
                self.unmap_in(&mut va_space, range_base, &uuid, (base, end), who);
            }
            return status;
        }
        first_time(
            &FIRST_MAPPING,
            "UVM_MAP_EXTERNAL_ALLOCATION: the first mapping made through the RM",
        );
        NV_OK
    }

    /// One GPU's half of [`UvmOpen::map_external`].
    #[allow(clippy::too_many_arguments)]
    fn map_on_gpu(
        &self,
        va_space: &mut VaSpace,
        range_base: u64,
        attributes: &[u8],
        h_client: u32,
        h_memory: u32,
        offset: u64,
        (base, end): (u64, u64),
        who: Who,
    ) -> u32 {
        let Some(read_only) = read_only(attributes) else {
            return NV_ERR_INVALID_ARGUMENT;
        };
        let uuid = uuid_at(attributes, 0);
        let Some(gpu_va_space) = va_space.gpu_va_spaces.get(&uuid) else {
            return NV_ERR_INVALID_DEVICE;
        };
        if h_client != gpu_va_space.h_client {
            other_client(h_client, gpu_va_space.h_client);
            return NV_ERR_NOT_SUPPORTED;
        }
        if attributes[20..ATTRIBUTES_SIZE].iter().any(|&b| b != 0) {
            first_time(
                &FIRST_KIND,
                "UVM_MAP_EXTERNAL_ALLOCATION: a caching type, format or compression asked for is left to the RM",
            );
        }
        self.unmap_in(va_space, range_base, &uuid, (base, end), who);
        let gpu_va_space = &va_space.gpu_va_spaces[&uuid];
        let Some((_open, by)) = gpu_va_space.ctl.caller(who) else {
            return nvctl::NV_ERR_INVALID_CLIENT;
        };
        let mapping = nvctl::ExternalMapping {
            h_client,
            h_device: gpu_va_space.h_device,
            h_va_space: gpu_va_space.h_va_space,
            h_memory,
            offset,
            base,
            length: end - base,
            read_only,
        };
        let h_virtual = match self.rm.map(&by, &mapping) {
            Ok(h_virtual) => h_virtual,
            Err(status) => return status,
        };
        va_space
            .ranges
            .get_mut(&range_base)
            .expect("the range is held")
            .maps
            .entry(uuid)
            .or_default()
            .insert(base, Mapping { end, h_virtual });
        NV_OK
    }

    /// `uvm_unmap_external_in_range`: every mapping on GPU `uuid` in the
    /// range at `range_base` that overlaps `[base, end)`. Linux splits one
    /// that only partly does and keeps the rest mapped; here it goes whole,
    /// as the RM's mapping is one object.
    fn unmap_in(
        &self,
        va_space: &mut VaSpace,
        range_base: u64,
        uuid: &Uuid,
        (base, end): (u64, u64),
        who: Who,
    ) {
        let Some(maps) = va_space
            .ranges
            .get_mut(&range_base)
            .and_then(|range| range.maps.get_mut(uuid))
        else {
            return;
        };
        let overlapping: Vec<u64> = maps
            .range(..end)
            .filter(|(_, map)| map.end > base)
            .map(|(&at, _)| at)
            .collect();
        let gone: Vec<Mapping> = overlapping
            .iter()
            .map(|at| maps.remove(at).expect("just found"))
            .collect();
        if maps.is_empty() {
            va_space
                .ranges
                .get_mut(&range_base)
                .expect("just found")
                .maps
                .remove(uuid);
        }
        if let Some(gpu_va_space) = va_space.gpu_va_spaces.get(uuid) {
            self.unmap(gpu_va_space, gone.into_iter(), who);
        }
    }

    /// `uvm_unmap_external`: `[base, base + length)` must lie in one
    /// external range, and the GPU be registered here.
    fn unmap_external(&self, params: &[u8], who: Who) -> u32 {
        let base = u64_at(params, 0);
        let Some(end) = range_end(base, u64_at(params, 8)) else {
            return NV_ERR_INVALID_ADDRESS;
        };
        let uuid = uuid_at(params, UNMAP_EXTERNAL_GPU);
        let mut va_space = self.va_space.lock();
        let Some(range_base) = range_holding(&va_space.ranges, base, end) else {
            return NV_ERR_INVALID_ADDRESS;
        };
        if !va_space.gpus.contains(&uuid) {
            return NV_ERR_INVALID_DEVICE;
        }
        self.unmap_in(&mut va_space, range_base, &uuid, (base, end), who);
        NV_OK
    }

    /// `uvm_api_alloc_semaphore_pool`: `length` bytes of zeroed system
    /// memory at `base`, whole 4 KiB pages overlapping no range here, mapped
    /// at `base` on each GPU with a GPU VA space now or later.
    /// `perGpuAttributes` says how a GPU it names maps the pool; each GPU
    /// named must be registered here, and at most one may cache it. When a
    /// GPU fails, what was mapped goes, and the pool with it.
    ///
    /// The pages are one contiguous block, which is what the RM can take as
    /// memory it did not allocate itself; Linux's may be scattered.
    fn alloc_semaphore_pool(&self, params: &[u8], who: Who) -> u32 {
        let base = u64_at(params, 0);
        let Some(end) = range_end(base, u64_at(params, 8)) else {
            return NV_ERR_INVALID_ADDRESS;
        };
        let count = u64_at(params, POOL_COUNT);
        if count > MAX_GPUS {
            return NV_ERR_INVALID_ARGUMENT;
        }
        let mut va_space = self.va_space.lock();
        if va_space.overlaps(base, end) {
            return NV_ERR_UVM_ADDRESS_IN_USE;
        }
        let mut attrs = BTreeMap::new();
        let mut cached = false;
        for i in 0..count as usize {
            let attributes = &params[POOL_ATTRIBUTES + i * ATTRIBUTES_SIZE..][..ATTRIBUTES_SIZE];
            let Some(gpu_attrs) = pool_attrs(attributes) else {
                return NV_ERR_INVALID_ARGUMENT;
            };
            let uuid = uuid_at(attributes, 0);
            if !va_space.gpus.contains(&uuid) {
                return NV_ERR_INVALID_DEVICE;
            }
            // At most one GPU, the pool's owner, may cache it.
            if gpu_attrs.cached && core::mem::replace(&mut cached, true) {
                return NV_ERR_INVALID_ARGUMENT;
            }
            attrs.insert(uuid, gpu_attrs);
        }
        let Some((memory, phys)) = zeroed_pages(end - base) else {
            return NV_ERR_NO_MEMORY;
        };
        let keep: Arc<dyn Any + Send + Sync> = Arc::new(memory.clone());
        let mut pool = SemaphorePool {
            end,
            memory,
            phys,
            keep,
            attrs,
            maps: BTreeMap::new(),
        };
        for (uuid, gpu_va_space) in &va_space.gpu_va_spaces {
            if let Err(status) = self.map_pool(gpu_va_space, uuid, base, &mut pool, who) {
                self.unmap_pool_everywhere(&va_space.gpu_va_spaces, pool, who);
                return status;
            }
        }
        va_space.pools.insert(base, pool);
        first_time(
            &FIRST_POOL,
            "UVM_ALLOC_SEMAPHORE_POOL: the first semaphore pool; its pages are the kernel's, lent to the RM on each GPU",
        );
        NV_OK
    }

    /// `uvm_mmap`, which takes only MAP_SHARED: what maps `len` bytes of the
    /// open from `offset`, which must be a VA space. A semaphore pool that is
    /// exactly that range maps its own pages (`uvm_mem_map_cpu_user`); a
    /// range that overlaps one here otherwise, of either kind, is in use;
    /// and any other is a range of managed memory, not served yet.
    ///
    /// Linux also refuses a mapping that is not where the offset says, or
    /// not both readable and writable; neither reaches here, and the pages
    /// are mapped where and as mmap was asked to.
    pub fn get_vmo(&self, offset: usize, len: usize) -> LxResult<Arc<VmObject>> {
        if !self.is_va_space() {
            return Err(LxError::EBADFD);
        }
        let base = offset as u64;
        let end = base.checked_add(len as u64).ok_or(LxError::EINVAL)?;
        let va_space = self.va_space.lock();
        if let Some(pool) = va_space.pools.get(&base).filter(|pool| pool.end == end) {
            first_time(
                &FIRST_POOL_CPU,
                "mmap: the first semaphore pool mapped for the CPU",
            );
            return Ok(pool.memory.clone());
        }
        if va_space.overlaps(base, end) {
            return Err(LxError::EADDRINUSE);
        }
        first_time(
            &FIRST_MANAGED,
            "mmap: a range of managed memory (cudaMallocManaged) is not served yet: ENODEV",
        );
        Err(LxError::ENODEV)
    }

    /// `uvm_api_validate_va_range`: whether `[base, base + length)` is
    /// exactly one range here, external or a semaphore pool. The range is
    /// not checked for whole pages, as there; one that is not exactly a
    /// range is logged with where it is, the first time.
    fn validate_va_range(&self, base: u64, length: u64) -> u32 {
        let found = self.va_space.lock().range_at(base);
        if found == Some((base, base.wrapping_add(length))) {
            first_time(
                &FIRST_VALIDATED,
                "UVM_VALIDATE_VA_RANGE: the first range validated",
            );
            return NV_OK;
        }
        not_a_range(base, length, found);
        NV_ERR_INVALID_ADDRESS
    }

    /// `uvm_register_channel`: on a GPU with a GPU VA space here, and once.
    /// UVM retains the channel and maps the buffers RM gives it into
    /// `[base, base + length)` of a VA space it owns; in one the RM owns the
    /// RM maps them itself, so the channel is only listed.
    fn register_channel(&self, params: &[u8]) -> u32 {
        let uuid = uuid_at(params, 0);
        let channel = (
            u32_at(params, REGISTER_CHANNEL_CLIENT),
            u32_at(params, REGISTER_CHANNEL_CHANNEL),
        );
        let mut va_space = self.va_space.lock();
        if !va_space.gpu_va_spaces.contains_key(&uuid) {
            return NV_ERR_INVALID_DEVICE;
        }
        if va_space
            .gpu_va_spaces
            .values()
            .any(|gpu_va_space| gpu_va_space.channels.contains(&channel))
        {
            return NV_ERR_INVALID_CHANNEL;
        }
        va_space
            .gpu_va_spaces
            .get_mut(&uuid)
            .expect("just found")
            .channels
            .insert(channel);
        first_time(
            &FIRST_CHANNEL,
            "UVM_REGISTER_CHANNEL: the first channel registered; its buffers are mapped by the RM",
        );
        NV_OK
    }

    /// `uvm_unregister_channel`: a channel registered on any GPU VA space
    /// here; the GPU named is not looked at, as on Linux.
    fn unregister_channel(&self, h_client: u32, h_channel: u32) -> u32 {
        let mut va_space = self.va_space.lock();
        let removed = va_space
            .gpu_va_spaces
            .values_mut()
            .any(|gpu_va_space| gpu_va_space.channels.remove(&(h_client, h_channel)));
        if removed {
            NV_OK
        } else {
            NV_ERR_INVALID_CHANNEL
        }
    }
}

/// `uvm_destroy_va_space`, of what is served: every mapping still made is
/// unmapped, as the close of the VA space. A semaphore pool's pages stay
/// while a CPU mapping or the RM still has them.
impl Drop for UvmOpen {
    fn drop(&mut self) {
        let va_space = core::mem::take(self.va_space.get_mut());
        for range in va_space.ranges.into_values() {
            for (uuid, maps) in range.maps {
                if let Some(gpu_va_space) = va_space.gpu_va_spaces.get(&uuid) {
                    self.unmap(gpu_va_space, maps.into_values(), Who::NOBODY);
                }
            }
        }
        for pool in va_space.pools.into_values() {
            self.unmap_pool_everywhere(&va_space.gpu_va_spaces, pool, Who::NOBODY);
        }
    }
}

/// One past the last byte of `[base, base + length)`, when the range is
/// one UVM takes (`uvm_api_range_invalid_4k`): whole 4 KiB pages, not at 0,
/// not empty and not wrapping.
fn range_end(base: u64, length: u64) -> Option<u64> {
    if base == 0 || length == 0 || !base.is_multiple_of(PAGE_4K) || !length.is_multiple_of(PAGE_4K)
    {
        return None;
    }
    base.checked_add(length)
}

/// The first byte of the external range that holds all of `[base, end)`.
fn range_holding(ranges: &BTreeMap<u64, ExternalRange>, base: u64, end: u64) -> Option<u64> {
    let (&at, range) = ranges.range(..=base).next_back()?;
    (range.end >= end).then_some(at)
}

/// Whether a mapping with these `UvmGpuMappingAttributes` is read-only, or
/// `None` when the module refuses them (`uvm_api_mapping_type_invalid`,
/// `uvm_api_caching_type_invalid`, `uvm_api_kind_type_invalid`).
fn read_only(attributes: &[u8]) -> Option<bool> {
    let mapping = u32_at(attributes, 16);
    let caching = u32_at(attributes, 20);
    let format = u32_at(attributes, 24);
    let element_bits = u32_at(attributes, 28);
    let compression = u32_at(attributes, 32);
    // Default, ReadWriteAtomic, ReadWrite, ReadOnly.
    let mapping_ok = mapping <= 3;
    // Default, ForceUncached, ForceCached.
    let caching_ok = caching <= 2;
    // Default and BlockLinear; Default and 8, 16, 32, 64 and 128 bits, each
    // a default only with the other.
    let kind_ok = compression <= 1
        && format <= 1
        && matches!(element_bits, 0 | 1 | 2 | 4 | 5 | 6)
        && (format == 0) == (element_bits == 0);
    (mapping_ok && caching_ok && kind_ok).then_some(mapping == 3)
}

/// How a GPU maps a semaphore pool with these `UvmGpuMappingAttributes`,
/// or `None` when the module refuses them (`uvm_mem_translate_gpu_attributes`,
/// which looks at no format or compression).
fn pool_attrs(attributes: &[u8]) -> Option<PoolAttrs> {
    let mapping = u32_at(attributes, 16);
    let caching = u32_at(attributes, 20);
    // Default, ReadWriteAtomic, ReadWrite, ReadOnly; and Default,
    // ForceUncached, ForceCached.
    (mapping <= 3 && caching <= 2).then_some(PoolAttrs {
        read_only: mapping == 3,
        cached: caching == 2,
    })
}

/// `length` bytes of zeroed, contiguous and pinned system memory, with the
/// physical address of its first byte. A length no machine has is no
/// memory, as there.
fn zeroed_pages(length: u64) -> Option<(Arc<VmObject>, u64)> {
    let pages = (length / PAGE_4K) as usize;
    let memory = VmObject::new_contiguous(pages, PAGE_4K.trailing_zeros() as usize).ok()?;
    let phys = memory.commit_page(0, MMUFlags::READ).ok()? as u64;
    Some((memory, phys))
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

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    u32_at(b, at) as i32
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
        cmd::INITIALIZE => "UVM_INITIALIZE",
        cmd::DEINITIALIZE => "UVM_DEINITIALIZE",
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
        39 => "UVM_PAGEABLE_MEM_ACCESS",
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
        75 => "UVM_MM_INITIALIZE",
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

/// The (command, status) pairs already logged.
static FAILED: Mutex<BTreeSet<(u32, u32)>> = Mutex::new(BTreeSet::new());

/// One log line the first time each served command fails with each status:
/// the CUDA error a program then reports is this line's doing.
fn failed(cmd: u32, status: u32) {
    if FAILED.lock().insert((cmd, status)) {
        zcore_drivers::klog_warn!("[nvidia-uvm] {} ({}) failed: {:#x}", name(cmd), cmd, status);
    }
}

/// The statuses the RM already refused a mapping's free with.
static UNMAP_REFUSED: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

/// One log line the first time the RM refuses to free a mapping's
/// NV50_MEMORY_VIRTUAL with each status: the mapping then stays until its
/// client is freed, at the latest when its `/dev/nvidiactl` open is closed.
fn unmap_refused(status: u32) {
    if UNMAP_REFUSED.lock().insert(status) {
        zcore_drivers::klog_warn!(
            "[nvidia-uvm] the RM refused to unmap a mapping: {:#x}; it stays until its client is freed",
            status
        );
    }
}

static FIRST_RM_OWNED: AtomicBool = AtomicBool::new(false);
static FIRST_GPU_VA_SPACE: AtomicBool = AtomicBool::new(false);
static FIRST_MAPPING: AtomicBool = AtomicBool::new(false);
static FIRST_KIND: AtomicBool = AtomicBool::new(false);
static FIRST_CHANNEL: AtomicBool = AtomicBool::new(false);
static FIRST_POOL: AtomicBool = AtomicBool::new(false);
static FIRST_POOL_CPU: AtomicBool = AtomicBool::new(false);
static FIRST_MANAGED: AtomicBool = AtomicBool::new(false);
static FIRST_VALIDATED: AtomicBool = AtomicBool::new(false);

/// `line` in the log, the first time it is said: how far a CUDA program got
/// shows in dmesg.
fn first_time(said: &AtomicBool, line: &str) {
    if !said.swap(true, Ordering::Relaxed) {
        zcore_drivers::klog_warn!("[nvidia-uvm] {}", line);
    }
}

/// One log line the first time UVM_VALIDATE_VA_RANGE is asked about a range
/// that is not exactly one here, with the range that holds its first byte,
/// if any: what libcuda expected to find there.
fn not_a_range(base: u64, length: u64, found: Option<(u64, u64)>) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    match found {
        Some((start, end)) => zcore_drivers::klog_warn!(
            "[nvidia-uvm] UVM_VALIDATE_VA_RANGE: [{:#x}, +{:#x}) is not a range; [{:#x}, {:#x}) holds its start: NV_ERR_INVALID_ADDRESS",
            base,
            length,
            start,
            end
        ),
        None => zcore_drivers::klog_warn!(
            "[nvidia-uvm] UVM_VALIDATE_VA_RANGE: [{:#x}, +{:#x}) is in no range: NV_ERR_INVALID_ADDRESS",
            base,
            length
        ),
    }
}

/// One log line the first time UVM_REGISTER_GPU_VASPACE names a fd that is
/// no open of `/dev/nvidiactl`, the only one an RM client is made through.
fn not_a_ctl_open(fd: i32) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        zcore_drivers::klog_warn!(
            "[nvidia-uvm] UVM_REGISTER_GPU_VASPACE: rmCtrlFd {} is no open of /dev/nvidiactl: NV_ERR_INVALID_ARGUMENT",
            fd
        );
    }
}

/// One log line the first time UVM_MAP_EXTERNAL_ALLOCATION names memory of
/// a client other than the GPU VA space's, which only Linux's UVM can map
/// (it dups the memory into a client of its own).
fn other_client(h_client: u32, expected: u32) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        zcore_drivers::klog_warn!(
            "[nvidia-uvm] UVM_MAP_EXTERNAL_ALLOCATION: memory of client {:#x}, the GPU VA space is client {:#x}'s: NV_ERR_NOT_SUPPORTED",
            h_client,
            expected
        );
    }
}

/// FERMI_VASPACE_A, the class of a GPU VA space.
const FERMI_VASPACE_A: u32 = 0x90f1;
/// Where in `NVOS21_PARAMETERS` and `NVOS64_PARAMETERS`, the two payloads of
/// NV_ESC_RM_ALLOC, the class and the pointer to the alloc params are.
const ALLOC_CLASS: usize = 12;
const ALLOC_PARAMS: usize = 16;
/// Where in `NV_VASPACE_ALLOCATION_PARAMETERS` the flags are.
const VASPACE_FLAGS: usize = 4;
/// NV_VASPACE_ALLOCATION_FLAGS_IS_EXTERNALLY_OWNED and
/// NV_VASPACE_ALLOCATION_FLAGS_ENABLE_PAGE_FAULTING.
const VASPACE_EXTERNALLY_OWNED: u32 = 1 << 3;
const VASPACE_PAGE_FAULTING: u32 = 1 << 6;

/// The flags of a VA space [`rm_owned_va_space`] changed, to put back once
/// the RM has made it.
#[must_use]
pub struct UvmVaSpaceFlags {
    /// The user address of the flags.
    at: usize,
    flags: u32,
}

impl UvmVaSpaceFlags {
    /// The flags as the caller wrote them, where the RM copied the params
    /// back out: the caller reads what it asked for. A caller that unmapped
    /// its params meanwhile gets nothing back.
    pub fn restore(self) {
        let _ = UserOutPtr::<u8>::from(self.at).write_array(&self.flags.to_le_bytes());
    }
}

/// With `nvidia.uvm`, an NV_ESC_RM_ALLOC of a FERMI_VASPACE_A libcuda means
/// to hand UVM (IS_EXTERNALLY_OWNED) goes to the RM as one the RM keeps the
/// page tables of, and without page faulting: its flags are cleared in the
/// caller's alloc params, which the RM copies in, and the flags it had come
/// back for [`UvmVaSpaceFlags::restore`] once the RM is done. `payload` is
/// the kernel copy of the escape's NVOS21 or NVOS64; any other allocation
/// is left alone.
pub fn rm_owned_va_space(payload: &[u8]) -> Option<UvmVaSpaceFlags> {
    if !nvctl::uvm_enabled() {
        return None;
    }
    strip_va_space_flags(payload)
}

/// [`rm_owned_va_space`] whatever the hatch says.
fn strip_va_space_flags(payload: &[u8]) -> Option<UvmVaSpaceFlags> {
    if payload.len() < ALLOC_PARAMS + 8 || u32_at(payload, ALLOC_CLASS) != FERMI_VASPACE_A {
        return None;
    }
    let params = u64_at(payload, ALLOC_PARAMS) as usize;
    if params == 0 {
        return None;
    }
    let at = params.checked_add(VASPACE_FLAGS)?;
    let flags = u32_at(&UserInPtr::<u8>::from(at).read_array(4).ok()?, 0);
    if flags & VASPACE_EXTERNALLY_OWNED == 0 {
        return None;
    }
    let rm_owned = flags & !(VASPACE_EXTERNALLY_OWNED | VASPACE_PAGE_FAULTING);
    UserOutPtr::<u8>::from(at)
        .write_array(&rm_owned.to_le_bytes())
        .ok()?;
    first_time(
        &FIRST_RM_OWNED,
        "a GPU VA space for UVM goes to the RM as the RM's: IS_EXTERNALLY_OWNED and ENABLE_PAGE_FAULTING cleared",
    );
    Some(UvmVaSpaceFlags { at, flags })
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
        "[nvidia-uvm] /dev/nvidia-uvm: cuInit's and cuCtxCreate's calls served, the rest logged"
    );
}

#[cfg(test)]
mod mapping_tests;
#[cfg(test)]
mod pool_tests;
#[cfg(test)]
mod tests;
