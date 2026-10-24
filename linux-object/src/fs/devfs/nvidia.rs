//! `/dev/nvidiactl` and `/dev/nvidia<N>` (hatch `nvidia.nvidiactl`, default
//! off): the device files NVIDIA's own user-mode driver talks to.
//!
//! libnvidia-ml (and through it `nvidia-smi`) and libcuda do not speak DRM.
//! They open `/dev/nvidiactl` (195:255) and one `/dev/nvidia<N>` (195:N) per
//! GPU, and drive the Resource Manager with ioctls whose NR is the escape.
//! This is `nvidia_ioctl` (kernel-open/nvidia/nv.c) and the non-RM half of
//! `RmIoctl` (escape.c), over the RM this kernel already carries, and it
//! keeps nv.c's rules:
//!
//! - the command is read for its NR and its size only, never its type or
//!   direction; a payload too big to encode comes through
//!   `NV_ESC_IOCTL_XFER_CMD`, which names the real escape, size and pointer;
//! - the payload is copied in, and copied back out even when the escape
//!   failed, unless what failed was a copy;
//! - every failure is EINVAL unless nv.c answers something else.
//!
//! Served here: CARD_INFO, CHECK_VERSION_STR, SYS_PARAMS, NUMA_INFO,
//! ATTACH_GPUS_TO_FD, WAIT_OPEN_COMPLETE, STATUS_CODE, REGISTER_FD and the OS
//! events (ALLOC_OS_EVENT, FREE_OS_EVENT, RM_GET_EVENT_DATA, and `poll`).
//! Served by the RM (`nvidia-rm-sys/vendor/eclipse_rm_frontend.c`): RM_ALLOC,
//! RM_FREE, RM_CONTROL, RM_DUP_OBJECT, RM_SHARE, RM_MAP_MEMORY,
//! RM_UNMAP_MEMORY, RM_VID_HEAP_CONTROL (cuMemAlloc / heap),
//! RM_UPDATE_DEVICE_MAPPING_INFO, and RM_ALLOC_MEMORY of an OS descriptor,
//! whose pages are pinned here first (`pin.rs`). Everything else answers
//! EINVAL and is logged the first time each escape is seen: RM_ALLOC_MEMORY
//! of any other class, SET_NUMA_STATUS and the dma-buf escapes.
//! CUDA also needs `/dev/nvidia-uvm` (`nvidia_uvm.rs`).
//!
//! A mapping is made in two steps, as on Linux. RM_MAP_MEMORY has the RM map
//! the memory and records the result on the open the client names in the
//! payload (`nv_add_mapping_context_to_file`); the client then mmaps that
//! open at offset 0, and gets exactly what was recorded (`nvidia_mmap`). An
//! open holds one mapping for as long as it lives, which is why libnvidia
//! opens a fresh file for each.
//!
//! So does an OS event. The client registers an open with ALLOC_OS_EVENT,
//! issued on that open and naming its own fd number for it, and binds an
//! `NV01_EVENT_OS_EVENT` in the RM to that number. What the RM posts to it
//! is queued on that open ([`post_event`], `nv_post_event`): it polls
//! readable, and RM_GET_EVENT_DATA reads the queue back. The registrations
//! are `nvidia_rm_sys::frontend::events`. The RM here is polled, so an event
//! fires only inside some RM call (a control that triggers one, or an RPC
//! that drains one the GSP queued); none arrives on an interrupt.
//!
//! The ioctl arrives from `sys_ioctl` (linux-syscall), not from
//! `INode::io_control`: it needs the caller's fd table (REGISTER_FD names a
//! fd, and the RM asks whether the caller has a GPU's node open), and from
//! there it also runs with no `File` lock held.
//!
//! Only a GPU the RM has state-loaded is listed or can be opened. One it
//! never brought up (the console GPU without `nvidia.console_gpu`) keeps its
//! minor, so `/dev/nvidia<N>` is the same GPU on every boot, but opening it
//! is EIO, which is what a failed `rm_init_adapter` gives on Linux.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::Waker;

use kernel_hal::user::{UserInPtr, UserOutPtr};
use kernel_hal::CachePolicy;
use lock::Mutex;
use rcore_fs::vfs::*;
use rcore_fs_devfs::{DevFS, DevINode};
use zcore_drivers::display::nvctl;
use zircon_object::vm::{pages, VmObject, PAGE_SIZE};

use crate::error::{LxError, LxResult};
use crate::fs::{poll_events_to_bus_mask, PollEvents};
use crate::sync::{subscribe_readiness_on, Event, EventBus, ReadinessSub};

/// `NV_MAJOR_DEVICE_NUMBER`.
pub const MAJOR: usize = 195;
/// `NV_MINOR_DEVICE_NUMBER_CONTROL_DEVICE`.
pub const CTL_MINOR: u32 = 255;
/// `NV_MINOR_DEVICE_NUMBER_REGULAR_MAX`: the last minor a GPU can have.
const GPU_MINOR_MAX: u32 = 247;

/// The escapes this file knows by name (nv-ioctl-numbers.h, nv_escape.h).
pub mod esc {
    pub const RM_ALLOC_MEMORY: u32 = 0x27;
    pub const RM_FREE: u32 = 0x29;
    pub const RM_CONTROL: u32 = 0x2A;
    pub const RM_ALLOC: u32 = 0x2B;
    pub const RM_DUP_OBJECT: u32 = 0x34;
    pub const RM_SHARE: u32 = 0x35;
    pub const RM_GET_EVENT_DATA: u32 = 0x52;
    pub const RM_VID_HEAP_CONTROL: u32 = 0x4A;
    pub const RM_MAP_MEMORY: u32 = 0x4E;
    pub const RM_UNMAP_MEMORY: u32 = 0x4F;
    pub const RM_UPDATE_DEVICE_MAPPING_INFO: u32 = 0x5E;
    pub const CARD_INFO: u32 = 200;
    pub const REGISTER_FD: u32 = 201;
    pub const ALLOC_OS_EVENT: u32 = 206;
    pub const FREE_OS_EVENT: u32 = 207;
    pub const STATUS_CODE: u32 = 209;
    pub const CHECK_VERSION_STR: u32 = 210;
    pub const IOCTL_XFER_CMD: u32 = 211;
    pub const ATTACH_GPUS_TO_FD: u32 = 212;
    pub const SYS_PARAMS: u32 = 214;
    pub const NUMA_INFO: u32 = 215;
    pub const WAIT_OPEN_COMPLETE: u32 = 218;
}

/// `NV_ABSOLUTE_MAX_IOCTL_SIZE` (nv.h): the largest payload XFER may name.
const MAX_IOCTL_SIZE: usize = 16384;

// The nv-ioctl.h payloads, x86_64. `SIZES` is in `nvctl::LAYOUT`'s order, and
// both are checked against the C `sizeof`s before any node exists.
const XFER_SIZE: usize = 16;
const CARD_INFO_SIZE: usize = 72;
const RM_API_VERSION_SIZE: usize = 72;
const SYS_PARAMS_SIZE: usize = 8;
const REGISTER_FD_SIZE: usize = 4;
const WAIT_OPEN_COMPLETE_SIZE: usize = 8;
const STATUS_CODE_SIZE: usize = 12;
/// `nv_ioctl_nvos33_parameters_with_fd`, and where in it the client names the
/// open to record the mapping on.
const MAP_MEMORY_SIZE: usize = 56;
const MAP_MEMORY_FD: usize = 48;
/// `nv_ioctl_alloc_os_event_t` and `nv_ioctl_free_os_event_t`: `hClient`,
/// `hDevice`, `fd`, `Status`.
const ALLOC_OS_EVENT_SIZE: usize = 16;
const FREE_OS_EVENT_SIZE: usize = 16;
/// `NVOS41_PARAMETERS`: `pEvent`, `MoreEvents`, `status`.
const GET_EVENT_DATA_SIZE: usize = 16;
/// `nv_ioctl_numa_info_t` (nv-ioctl-numa.h), and where in it the count of
/// retired pages is, after the 64 addresses it can hold.
const NUMA_INFO_SIZE: usize = 560;
const NUMA_INFO_OFFLINE_COUNT: usize = 552;
/// `nv_ioctl_nvos02_parameters_with_fd`, and where in it the address of the
/// memory an OS descriptor names is. `hClass`, `flags` and `limit` sit
/// around it, as NVOS02_PARAMETERS lays them out.
const ALLOC_MEMORY_SIZE: usize = 56;
const ALLOC_MEMORY_PMEMORY: usize = 24;
const ALLOC_MEMORY_CLASS: usize = 12;
const ALLOC_MEMORY_FLAGS: usize = 16;
const ALLOC_MEMORY_LIMIT: usize = 32;
const ALLOC_MEMORY_STATUS: usize = 40;
const SIZES: [usize; 17] = [
    XFER_SIZE,
    CARD_INFO_SIZE,
    RM_API_VERSION_SIZE,
    SYS_PARAMS_SIZE,
    REGISTER_FD_SIZE,
    WAIT_OPEN_COMPLETE_SIZE,
    STATUS_CODE_SIZE,
    MAP_MEMORY_SIZE,
    MAP_MEMORY_FD,
    ALLOC_OS_EVENT_SIZE,
    FREE_OS_EVENT_SIZE,
    GET_EVENT_DATA_SIZE,
    nvctl::UnixEvent::SIZE,
    NUMA_INFO_SIZE,
    NUMA_INFO_OFFLINE_COUNT,
    ALLOC_MEMORY_SIZE,
    ALLOC_MEMORY_PMEMORY,
];

/// The most events queued on one open that nobody has read. Linux queues
/// until an atomic allocation fails and then drops the event without a
/// wake-up; here the queue stops at this, the same way.
const MAX_QUEUED_EVENTS: usize = 1024;

const NV_OK: u32 = 0;
/// What `os_memcpy_to_user` answers when the copy faults.
const NV_ERR_INVALID_ADDRESS: u32 = 0x1E;
const NV_ERR_INVALID_ARGUMENT: u32 = 0x1F;
const NV_ERR_NOT_READY: u32 = 0x55;
const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
const NV_ERR_OPERATING_SYSTEM: u32 = 0x59;
const PCI_VENDOR_NVIDIA: u16 = 0x10de;
/// `NV0000_CTRL_NO_NUMA_NODE`, `NV_IOCTL_NUMA_STATUS_DISABLED` and
/// `NV_IOCTL_NUMA_INFO_MAX_OFFLINE_ADDRESSES`.
const NO_NUMA_NODE: i32 = -1;
const NUMA_STATUS_DISABLED: u32 = 0;
const NUMA_MAX_OFFLINE: u32 = 64;

/// `_IOC_NR` and `_IOC_SIZE`: all `nvidia_ioctl` reads of the command.
pub fn decode(cmd: u32) -> (u32, usize) {
    (cmd & 0xff, ((cmd >> 16) & 0x3fff) as usize)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// `nv_ioctl_xfer_t`: the escape, its size and where its payload is.
fn parse_xfer(b: &[u8]) -> (u32, usize, usize) {
    (u32_at(b, 0), u32_at(b, 4) as usize, u64_at(b, 8) as usize)
}

/// Which node an open is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Node {
    Ctl,
    Gpu(u32),
}

impl Node {
    fn minor(self) -> u32 {
        match self {
            Node::Ctl => CTL_MINOR,
            Node::Gpu(minor) => minor,
        }
    }
}

fn node_metadata(node: Node, inode_id: usize) -> Metadata {
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
        // What nvidia-modprobe creates them as.
        mode: 0o666,
        nlinks: 1,
        uid: 0,
        gid: 0,
        rdev: make_rdev(MAJOR, node.minor() as usize),
    }
}

/// `/dev/nvidiactl` or `/dev/nvidia<N>` in the device filesystem. Opening it
/// makes an [`NvidiaOpen`], which is what the fd holds.
pub struct NvidiaDev {
    node: Node,
    inode_id: usize,
}

impl NvidiaDev {
    fn new(node: Node) -> Self {
        NvidiaDev {
            node,
            inode_id: DevFS::new_inode_id(),
        }
    }

    /// `nvidia_open`. The control device always opens. A GPU opens once the
    /// RM has state-loaded it; before that it is EIO (a failed
    /// `rm_init_adapter`), and a minor that names no GPU is ENODEV
    /// (`find_minor_locked`). Every open completes before `open(2)` returns,
    /// so WAIT_OPEN_COMPLETE never has anything to wait for.
    pub fn open_client(&self) -> Result<Arc<dyn INode>> {
        if let Node::Gpu(minor) = self.node {
            match nvctl::cards().get(minor as usize) {
                None => return Err(FsError::NoDevice),
                Some(card) if card.instance.is_none() => return Err(FsError::DeviceError),
                Some(_) => {}
            }
        }
        Ok(Arc::new(NvidiaOpen::new(self.node, self.inode_id)))
    }
}

impl INode for NvidiaDev {
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
        Ok(node_metadata(self.node, self.inode_id))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// What `nvidia_ioctl` needs to know about whoever is calling, which only
/// the syscall layer can see.
pub struct NvCaller<'a> {
    /// The caller's effective uid is 0.
    pub admin: bool,
    /// The RM instances of every `/dev/nvidia<N>` the calling process has
    /// open (`nv_is_gpu_accessible`'s `iterate_fd`), see [`gpu_mask`]. Only
    /// asked for when the escape reaches the RM.
    pub gpu_mask: &'a dyn Fn() -> u32,
    /// The inode behind one of the caller's fds, for REGISTER_FD.
    pub open_of_fd: &'a dyn Fn(i32) -> Option<Arc<dyn INode>>,
    /// `(address, pages, for writing)` of the caller's memory, pinned
    /// ([`pin_user_pages`]), for RM_ALLOC_MEMORY of an OS descriptor.
    pub pin_user_pages: &'a dyn Fn(usize, usize, bool) -> LxResult<nvctl::UserPages>,
}

/// Serialises REGISTER_FD's check-and-link, which Linux runs under the RM
/// API lock: two opens linking to each other at once would otherwise both
/// see the other unlinked, and the pair would keep each other alive forever.
static REGISTER_LOCK: Mutex<()> = Mutex::new(());

/// `nv_ctl_device.numa_memblock_size`: set by the first SYS_PARAMS.
static MEMBLOCK_SIZE: AtomicU64 = AtomicU64::new(0);

/// One open of `/dev/nvidiactl` or `/dev/nvidia<N>` (`nv_file_private_t`).
///
/// Its address is the `clientOSInfo` the RM files this open's clients
/// under, and dropping it (the last close) frees them, as
/// `rm_cleanup_file_private` does.
pub struct NvidiaOpen {
    node: Node,
    inode_id: usize,
    state: Mutex<OpenState>,
    events: Arc<OpenEvents>,
}

/// The OS events queued on one open: `nvlfp`'s `event_data_head`,
/// `dataless_event_pending` and `waitqueue`.
#[derive(Default)]
struct OpenEvents {
    queue: Mutex<EventQueue>,
    /// READABLE exactly while [`OpenEvents::ready`] would answer yes.
    bus: Arc<Mutex<EventBus>>,
}

#[derive(Default)]
struct EventQueue {
    /// The events with data, oldest first.
    data: VecDeque<nvctl::UnixEvent>,
    /// An event without data came since the last `poll` that reported one.
    dataless: bool,
}

impl EventQueue {
    fn ready(&self) -> bool {
        !self.data.is_empty() || self.dataless
    }
}

impl OpenEvents {
    /// `nv_post_event`: an event with data is queued, one without is
    /// remembered, and pollers wake. One past [`MAX_QUEUED_EVENTS`] is
    /// dropped, and wakes nobody.
    fn post(&self, event: Option<nvctl::UnixEvent>) {
        let mut queue = self.queue.lock();
        match event {
            Some(_) if queue.data.len() >= MAX_QUEUED_EVENTS => {
                dropped_event();
                return;
            }
            Some(event) => queue.data.push_back(event),
            None => queue.dataless = true,
        }
        self.bus.lock().set(Event::READABLE);
    }

    /// `nvidia_poll`: readable while there is an event to report, and
    /// reporting an event without data consumes it.
    fn poll(&self) -> bool {
        let mut queue = self.queue.lock();
        let ready = queue.ready();
        queue.dataless = false;
        self.settle(&queue);
        ready
    }

    /// `nv_get_event`: the oldest event with data, and whether another one
    /// follows. An event without data is not one, and stays for `poll`.
    fn take(&self) -> Option<(nvctl::UnixEvent, bool)> {
        let mut queue = self.queue.lock();
        let event = queue.data.pop_front()?;
        self.settle(&queue);
        Some((event, !queue.data.is_empty()))
    }

    /// Keep the bus's READABLE on exactly while `queue` has something to
    /// report: a poller parked on a flag left on would wake for nothing, over
    /// and over.
    fn settle(&self, queue: &EventQueue) {
        if !queue.ready() {
            self.bus.lock().clear(Event::READABLE);
        }
    }
}

/// The opens an OS event can be posted to, by their `clientOSInfo`: every
/// open that has issued an ALLOC_OS_EVENT and is still open. [`post_event`]
/// finds the queue here rather than following the token back to the open.
static EVENT_OPENS: Mutex<BTreeMap<usize, Arc<OpenEvents>>> = Mutex::new(BTreeMap::new());

/// `nv_post_event`, as the RM's registrations hand it an event for the open
/// `os_info`. An open that has closed takes nothing.
fn post_event(os_info: usize, event: Option<nvctl::UnixEvent>) {
    let events = EVENT_OPENS.lock().get(&os_info).cloned();
    if let Some(events) = events {
        events.post(event);
    }
}

#[derive(Default)]
struct OpenState {
    /// ATTACH_GPUS_TO_FD's list (`nvlfp->attached_gpus`), set at most once.
    attached_gpus: Option<Vec<u32>>,
    /// REGISTER_FD's control open (`nvfp->ctl_nvfp`), held for as long as
    /// this open lives, as Linux holds `ctl_nvfp_priv`.
    ctl: Option<Arc<dyn INode>>,
    /// The mapping an RM_MAP_MEMORY recorded on this open
    /// (`nvlfp->mmap_context`), at most one, for as long as it lives.
    mapping: Option<nvctl::MmapContext>,
}

impl NvidiaOpen {
    fn new(node: Node, inode_id: usize) -> Self {
        NvidiaOpen {
            node,
            inode_id,
            state: Mutex::new(OpenState::default()),
            events: Arc::new(OpenEvents::default()),
        }
    }

    fn os_info(&self) -> usize {
        self as *const Self as usize
    }

    fn is_ctl(&self) -> bool {
        self.node == Node::Ctl
    }

    /// `RmIoctl`'s `clientOSInfo`: the control open REGISTER_FD linked this
    /// one to (`nvfp->ctl_nvfp`), or this open itself. Clients are only made
    /// through a control open, and the RM refuses a call that names one with
    /// any other `clientOSInfo` (NV_ERR_INVALID_CLIENT), as it refused
    /// cuCtxCreate's NV01_EVENT_OS_EVENT on `/dev/nvidia<N>` (CUDA's 400).
    /// The OS events this open registers stay its own.
    fn client_os_info(&self) -> usize {
        let state = self.state.lock();
        let ctl = state
            .ctl
            .as_ref()
            .and_then(|ctl| ctl.downcast_ref::<NvidiaOpen>());
        ctl.map_or(self.os_info(), NvidiaOpen::os_info)
    }

    /// What UVM makes its RM calls on a client of this open through (its
    /// `clientOSInfo`), when it is an open of `/dev/nvidiactl`, the only
    /// open a client is made through.
    pub(super) fn uvm_os_info(&self) -> Option<usize> {
        self.is_ctl().then(|| self.os_info())
    }

    /// An open of `/dev/nvidiactl`, or of `/dev/nvidia0`, for the UVM tests.
    #[cfg(test)]
    pub(super) fn for_uvm_test(ctl: bool) -> Self {
        NvidiaOpen::new(if ctl { Node::Ctl } else { Node::Gpu(0) }, 1)
    }

    /// `NV_CTL_DEVICE_ONLY`.
    fn ctl_only(&self) -> LxResult<()> {
        if self.is_ctl() {
            Ok(())
        } else {
            Err(LxError::EINVAL)
        }
    }

    /// `NV_ACTUAL_DEVICE_ONLY`.
    fn gpu_only(&self) -> LxResult<()> {
        if self.is_ctl() {
            Err(LxError::EINVAL)
        } else {
            Ok(())
        }
    }

    /// The GPU minor this is an open of, or `None` for `/dev/nvidiactl`.
    pub fn gpu_minor(&self) -> Option<u32> {
        match self.node {
            Node::Ctl => None,
            Node::Gpu(minor) => Some(minor),
        }
    }

    /// `nvidia_ioctl`.
    pub fn ioctl(&self, cmd: u32, arg: usize, caller: &NvCaller) -> LxResult<usize> {
        let (mut nr, mut size) = decode(cmd);
        let mut ptr = arg;
        if nr == esc::IOCTL_XFER_CMD {
            if size != XFER_SIZE {
                return Err(LxError::EINVAL);
            }
            let xfer = UserInPtr::<u8>::from(arg)
                .read_array(XFER_SIZE)
                .map_err(|_| LxError::EFAULT)?;
            (nr, size, ptr) = parse_xfer(&xfer);
            if size > MAX_IOCTL_SIZE {
                return Err(LxError::EINVAL);
            }
        }
        let mut payload = UserInPtr::<u8>::from(ptr)
            .read_array(size)
            .map_err(|_| LxError::EFAULT)?;
        let ret = self.escape(nr, &mut payload, caller);
        if ret != Err(LxError::EFAULT) {
            UserOutPtr::<u8>::from(ptr)
                .write_array(&payload)
                .map_err(|_| LxError::EFAULT)?;
        }
        ret.map(|()| 0)
    }

    /// The `switch` of `nvidia_ioctl`, on the kernel copy of the payload.
    fn escape(&self, nr: u32, payload: &mut [u8], caller: &NvCaller) -> LxResult<()> {
        match nr {
            esc::WAIT_OPEN_COMPLETE => {
                if payload.len() != WAIT_OPEN_COMPLETE_SIZE {
                    return Err(LxError::EINVAL);
                }
                // `open_rc` 0 and `adapter_status` NV_OK: only an open that
                // succeeded exists here.
                payload.fill(0);
                Ok(())
            }
            esc::CARD_INFO => {
                self.ctl_only()?;
                card_info(payload, &listed_cards())
            }
            esc::ATTACH_GPUS_TO_FD => {
                self.ctl_only()?;
                self.attach_gpus(payload)
            }
            esc::CHECK_VERSION_STR => {
                self.ctl_only()?;
                check_version(payload, nvctl::rm_version())
            }
            esc::SYS_PARAMS => {
                self.ctl_only()?;
                sys_params(payload, &MEMBLOCK_SIZE)
            }
            esc::NUMA_INFO => {
                self.gpu_only()?;
                numa_info(payload, MEMBLOCK_SIZE.load(Ordering::SeqCst))
            }
            _ => self.rm_ioctl(nr, payload, caller),
        }
    }

    /// `rm_ioctl` -> `RmIoctl`: its non-RM escapes here, the RM ones to the
    /// RM. Any status but NV_OK is EINVAL, as `nvidia_ioctl` maps it.
    fn rm_ioctl(&self, nr: u32, payload: &mut [u8], caller: &NvCaller) -> LxResult<()> {
        match nr {
            esc::STATUS_CODE => {
                if payload.len() != STATUS_CODE_SIZE {
                    return Err(LxError::EINVAL);
                }
                self.ctl_only()?;
                status_code(payload, &nvctl::cards())
            }
            esc::REGISTER_FD => {
                if payload.len() != REGISTER_FD_SIZE {
                    return Err(LxError::EINVAL);
                }
                self.register_fd(u32_at(payload, 0) as i32, caller)
            }
            esc::RM_ALLOC => {
                if !nvctl::rm_up() {
                    no_rm_yet();
                    return Err(LxError::EINVAL);
                }
                // A VA space libcuda means to hand UVM goes to the RM as one
                // the RM keeps the page tables of (nvidia_uvm.rs).
                let uvm_flags = super::nvidia_uvm::rm_owned_va_space(payload);
                let status = nvctl::rm_escape(nr, payload, &self.rm_caller(caller));
                if let Some(flags) = uvm_flags {
                    flags.restore();
                }
                escape_result(nr, status)
            }
            esc::RM_ALLOC_MEMORY | esc::RM_VID_HEAP_CONTROL => {
                if !nvctl::rm_up() {
                    no_rm_yet();
                    return Err(LxError::EINVAL);
                }
                // An OS descriptor names memory of the caller's: its pages
                // are pinned here, outside the RM gate, and lent to the RM,
                // which takes them in `os_lock_user_pages`.
                let pages = pin_os_descriptor(payload, self.is_ctl(), caller);
                let status = nvctl::rm_escape_lending(nr, payload, &self.rm_caller(caller), pages);
                if status == NV_OK && nr == esc::RM_ALLOC_MEMORY {
                    alloc_memory_refused(payload);
                }
                escape_result(nr, status)
            }
            esc::RM_FREE
            | esc::RM_CONTROL
            | esc::RM_DUP_OBJECT
            | esc::RM_SHARE
            | esc::RM_UNMAP_MEMORY
            | esc::RM_UPDATE_DEVICE_MAPPING_INFO => {
                if !nvctl::rm_up() {
                    no_rm_yet();
                    return Err(LxError::EINVAL);
                }
                escape_result(nr, nvctl::rm_escape(nr, payload, &self.rm_caller(caller)))
            }
            esc::RM_MAP_MEMORY => self.map_memory(payload, caller),
            esc::ALLOC_OS_EVENT | esc::FREE_OS_EVENT | esc::RM_GET_EVENT_DATA => {
                if !nvctl::rm_up() {
                    no_rm_yet();
                    return Err(LxError::EINVAL);
                }
                self.os_event(nr, payload)
            }
            _ => {
                not_served(nr);
                Err(LxError::EINVAL)
            }
        }
    }

    /// The three OS-event escapes, which `rm_ioctl` serves before `RmIoctl`
    /// and outside the RM core: a payload of the wrong size is EINVAL, and
    /// otherwise the answer is in the payload's status. Like Linux, any open
    /// takes them, control or GPU.
    fn os_event(&self, nr: u32, payload: &mut [u8]) -> LxResult<()> {
        match nr {
            esc::ALLOC_OS_EVENT => {
                if payload.len() != ALLOC_OS_EVENT_SIZE {
                    return Err(LxError::EINVAL);
                }
                // Reachable before the RM can know about the registration,
                // so no event posted to it is ever lost.
                EVENT_OPENS
                    .lock()
                    .insert(self.os_info(), self.events.clone());
                let status =
                    nvctl::alloc_os_event(u32_at(payload, 0), self.os_info(), u32_at(payload, 8));
                put_u32(payload, 12, status);
            }
            esc::FREE_OS_EVENT => {
                if payload.len() != FREE_OS_EVENT_SIZE {
                    return Err(LxError::EINVAL);
                }
                let status = nvctl::free_os_event(u32_at(payload, 0), u32_at(payload, 8));
                put_u32(payload, 12, status);
            }
            _ => {
                if payload.len() != GET_EVENT_DATA_SIZE {
                    return Err(LxError::EINVAL);
                }
                get_event_data(payload, &self.events);
            }
        }
        Ok(())
    }

    /// Who the RM is told is calling.
    fn rm_caller(&self, caller: &NvCaller) -> nvctl::Caller {
        nvctl::Caller {
            os_info: self.client_os_info(),
            ctl: self.is_ctl(),
            admin: caller.admin,
            gpu_mask: (caller.gpu_mask)(),
        }
    }

    /// NV_ESC_RM_MAP_MEMORY: the RM maps, and the mapping is recorded on the
    /// open behind the payload's `fd`, or unmapped again with the reason as
    /// the call's status. That open is looked up before the RM is entered,
    /// so nothing of the fd table is ever taken under the RM gate.
    fn map_memory(&self, payload: &mut [u8], caller: &NvCaller) -> LxResult<()> {
        if !nvctl::rm_up() {
            no_rm_yet();
            return Err(LxError::EINVAL);
        }
        let target = if payload.len() == MAP_MEMORY_SIZE {
            (caller.open_of_fd)(u32_at(payload, MAP_MEMORY_FD) as i32)
        } else {
            None
        };
        let rm_caller = self.rm_caller(caller);
        let (status, mapping) = nvctl::rm_map_memory(payload, &rm_caller);
        if let Some(mapping) = mapping {
            let cards = nvctl::cards();
            let instance_of = |minor: u32| cards.get(minor as usize).and_then(|c| c.instance);
            if let Err(why) = record_mapping(target.as_deref(), mapping, &instance_of) {
                nvctl::map_unrecorded(payload, &rm_caller, why);
            }
        }
        escape_result(esc::RM_MAP_MEMORY, status)
    }

    /// `nvidia_mmap`: the mapping RM_MAP_MEMORY recorded on this open, as one
    /// physical VMO of exactly the length asked for. An open with none was
    /// never validated by the RM, and maps nothing.
    pub fn get_vmo(&self, offset: usize, len: usize) -> LxResult<Arc<VmObject>> {
        let Some(mapping) = self.state.lock().mapping else {
            invalid_mmap();
            return Err(LxError::EINVAL);
        };
        let (start, pages, policy) = mmap_window(&mapping, offset, len)?;
        let vmo = VmObject::new_physical(start, pages);
        // A VMO nobody has mapped yet always takes a policy; Linux answers a
        // caching it cannot encode with ENXIO.
        vmo.set_cache_policy(policy).map_err(|_| LxError::ENXIO)?;
        Ok(vmo)
    }

    /// Whether the mapping recorded on this open is one the RM granted
    /// read-only, which `nvidia_mmap` installs read-only however it was asked
    /// for, and with no way back to writable (`VM_WRITE` and `VM_MAYWRITE`
    /// cleared).
    pub fn mmap_read_only(&self) -> bool {
        self.state.lock().mapping.is_some_and(|m| !m.writable)
    }

    /// NV_ESC_ATTACH_GPUS_TO_FD: the GPUs this control open keeps attached.
    /// Every non-zero id must name a GPU this kernel lists (`nvidia_dev_get`),
    /// or the list is not kept and can be sent again.
    fn attach_gpus(&self, payload: &[u8]) -> LxResult<()> {
        if payload.is_empty() || !payload.len().is_multiple_of(4) {
            return Err(LxError::EINVAL);
        }
        let ids: Vec<u32> = (0..payload.len() / 4)
            .map(|i| u32_at(payload, i * 4))
            .collect();
        // The ids come from the RM; asked before this open's lock is taken.
        let cards = listed_cards();
        let mut state = self.state.lock();
        if state.attached_gpus.is_some() {
            return Err(LxError::EINVAL);
        }
        let known = |id: u32| cards.iter().any(|&(_, gpu_id)| gpu_id == id);
        if ids.iter().any(|&id| id != 0 && !known(id)) {
            return Err(LxError::EINVAL);
        }
        state.attached_gpus = Some(ids);
        Ok(())
    }

    /// NV_ESC_REGISTER_FD: link this open to the control open behind
    /// `ctl_fd`, once. The target must be a `/dev/nvidiactl` open that is
    /// neither this one nor linked itself.
    fn register_fd(&self, ctl_fd: i32, caller: &NvCaller) -> LxResult<()> {
        let target = (caller.open_of_fd)(ctl_fd).ok_or(LxError::EINVAL)?;
        let _serial = REGISTER_LOCK.lock();
        {
            let ctl = target
                .downcast_ref::<NvidiaOpen>()
                .filter(|open| open.is_ctl())
                .ok_or(LxError::EINVAL)?;
            if self.state.lock().ctl.is_some() {
                return Err(LxError::EINVAL);
            }
            if core::ptr::eq(ctl, self) || ctl.state.lock().ctl.is_some() {
                return Err(LxError::EINVAL);
            }
        }
        self.state.lock().ctl = Some(target);
        Ok(())
    }
}

impl NvidiaOpen {
    /// Park a poller until there is an OS event to report on this open.
    pub fn subscribe_readiness(&self, events: PollEvents, waker: &Waker) -> ReadinessSub {
        subscribe_readiness_on(&self.events.bus, poll_events_to_bus_mask(events), waker)
    }
}

impl Drop for NvidiaOpen {
    fn drop(&mut self) {
        // Out of reach of a post first: the RM's registrations still name
        // this open until `release` frees them.
        EVENT_OPENS.lock().remove(&self.os_info());
        nvctl::release(self.os_info());
    }
}

impl INode for NvidiaOpen {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::InvalidParam)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::InvalidParam)
    }

    /// `nvidia_poll`: readable while an OS event is queued here. Linux
    /// reports `POLLPRI` with `POLLIN`; nothing here reports `POLLPRI`, so a
    /// client that polls for it alone is never told.
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: self.events.poll(),
            ..PollStatus::default()
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(node_metadata(self.node, self.inode_id))
    }

    /// Never reached: `sys_ioctl` calls [`NvidiaOpen::ioctl`] with the
    /// caller's context, which this signature has no room for.
    fn io_control(&self, _cmd: u32, _data: usize) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// NV_ESC_RM_GET_EVENT_DATA (`get_os_event_data`): the oldest event queued
/// on the open, copied out to the payload's `pEvent`, and whether another
/// follows. An empty queue answers `NV_ERR_OPERATING_SYSTEM` and leaves
/// `MoreEvents` as it came; an event that cannot be copied out is lost, as
/// on Linux, which takes it off the queue first.
fn get_event_data(payload: &mut [u8], events: &OpenEvents) {
    let status = match events.take() {
        None => NV_ERR_OPERATING_SYSTEM,
        Some((event, more)) => {
            put_u32(payload, 8, more as u32);
            match UserOutPtr::<u8>::from(u64_at(payload, 0) as usize).write_array(&event.to_bytes())
            {
                Ok(()) => NV_OK,
                Err(_) => NV_ERR_INVALID_ADDRESS,
            }
        }
    };
    put_u32(payload, 12, status);
}

/// One log line the first time an open's queue is full and an event is
/// dropped: a client that registered for events and never reads them.
fn dropped_event() {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        zcore_drivers::klog_warn!(
            "[nvidiactl] {} OS events queued on one open and none read: dropping",
            MAX_QUEUED_EVENTS
        );
    }
}

/// `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` (cl0071.h).
const OS_DESCRIPTOR_CLASS: u32 = 0x71;
/// `NVOS02_FLAGS_ALLOC_USER_READ_ONLY` (nvos.h, bit 21).
const ALLOC_USER_READ_ONLY: u32 = 1 << 21;

/// `NVOS32_PARAMETERS` layout for `RM_VID_HEAP_CONTROL`.
const VID_HEAP_CONTROL_SIZE: usize = 184;
const VID_HEAP_FUNCTION: usize = 8;
const VID_HEAP_FUNCTION_ALLOC_OS_DESCRIPTOR: u32 = 27;
const VID_HEAP_ATTR2: usize = 56;
const VID_HEAP_DESCRIPTOR: usize = 64;
const VID_HEAP_LIMIT: usize = 72;
const VID_HEAP_ATTR2_PROTECTION_USER_RO: u32 = 0x400000;

/// The pages an RM_ALLOC_MEMORY or RM_VID_HEAP_CONTROL payload has the RM pin,
/// as RmCreateOsDescriptor asks `os_lock_user_pages` for them: `(address,
/// pages, for writing)`. `None` for a call that never gets that far: a
/// payload of the wrong size, the control device, another class/function, an
/// address that is not page aligned, or a limit that overflows.
fn os_descriptor_pages(payload: &[u8], ctl: bool) -> Option<(usize, usize, bool)> {
    if ctl {
        return None;
    }
    if payload.len() == ALLOC_MEMORY_SIZE
        && u32_at(payload, ALLOC_MEMORY_CLASS) == OS_DESCRIPTOR_CLASS
    {
        let address = u64_at(payload, ALLOC_MEMORY_PMEMORY);
        if !address.is_multiple_of(PAGE_SIZE as u64) {
            return None;
        }
        let size = u64_at(payload, ALLOC_MEMORY_LIMIT).checked_add(1)?;
        let count = 1 + (size - 1) / PAGE_SIZE as u64;
        let write = u32_at(payload, ALLOC_MEMORY_FLAGS) & ALLOC_USER_READ_ONLY == 0;
        return Some((
            usize::try_from(address).ok()?,
            usize::try_from(count).ok()?,
            write,
        ));
    }
    if payload.len() == VID_HEAP_CONTROL_SIZE
        && u32_at(payload, VID_HEAP_FUNCTION) == VID_HEAP_FUNCTION_ALLOC_OS_DESCRIPTOR
    {
        let address = u64_at(payload, VID_HEAP_DESCRIPTOR);
        if !address.is_multiple_of(PAGE_SIZE as u64) {
            return None;
        }
        let size = u64_at(payload, VID_HEAP_LIMIT).checked_add(1)?;
        let count = 1 + (size - 1) / PAGE_SIZE as u64;
        let write = u32_at(payload, VID_HEAP_ATTR2) & VID_HEAP_ATTR2_PROTECTION_USER_RO == 0;
        return Some((
            usize::try_from(address).ok()?,
            usize::try_from(count).ok()?,
            write,
        ));
    }
    None
}

/// The pages of the OS descriptor an RM_ALLOC_MEMORY payload names, pinned
/// for the RM to take; `None` for any other call, and for pages that could
/// not be pinned.
fn pin_os_descriptor(payload: &[u8], ctl: bool, caller: &NvCaller) -> Option<nvctl::UserPages> {
    let (va, count, write) = os_descriptor_pages(payload, ctl)?;
    match (caller.pin_user_pages)(va, count, write) {
        Ok(pages) => Some(pages),
        Err(e) => {
            unpinned(va, count, write, e);
            None
        }
    }
}

/// One log line for each OS descriptor whose pages could not be pinned: the
/// RM then refuses the call in its status (its `os_lock_user_pages` answers
/// `NV_ERR_INVALID_ADDRESS`, and there is no I/O memory to fall back to),
/// which no failed syscall shows.
fn unpinned(va: usize, count: usize, write: bool, e: LxError) {
    zcore_drivers::klog_warn!(
        "[nvidiactl] RM_ALLOC_MEMORY: {} pages at {:#x} not pinned for {}: {:?}",
        count,
        va,
        if write { "writing" } else { "reading" },
        e
    );
}

/// One log line for an RM_ALLOC_MEMORY the RM refused in its status, which
/// no failed syscall shows either: what the client asked for, and the RM's
/// answer.
fn alloc_memory_refused(payload: &[u8]) {
    if payload.len() != ALLOC_MEMORY_SIZE || u32_at(payload, ALLOC_MEMORY_STATUS) == NV_OK {
        return;
    }
    zcore_drivers::klog_warn!(
        "[nvidiactl] RM_ALLOC_MEMORY of class {:#x}, flags {:#x}, limit {:#x} at {:#x}: RM status {:#x}",
        u32_at(payload, ALLOC_MEMORY_CLASS),
        u32_at(payload, ALLOC_MEMORY_FLAGS),
        u64_at(payload, ALLOC_MEMORY_LIMIT),
        u64_at(payload, ALLOC_MEMORY_PMEMORY),
        u32_at(payload, ALLOC_MEMORY_STATUS)
    );
}

/// `rm_ioctl`'s answer for an RM escape: any status but NV_OK is EINVAL, as
/// `nvidia_ioctl` maps it, and one the RM does not serve is logged.
fn escape_result(nr: u32, status: u32) -> LxResult<()> {
    match status {
        NV_OK => Ok(()),
        NV_ERR_NOT_SUPPORTED => {
            not_served(nr);
            Err(LxError::EINVAL)
        }
        _ => Err(LxError::EINVAL),
    }
}

/// `nv_add_mapping_context_to_file`: `mapping` recorded on `target`, the
/// open behind the fd RM_MAP_MEMORY named. It must be an NVIDIA open of the
/// node the mapping is for -- `/dev/nvidiactl` for system memory
/// (`nv_get_file_private` with `ctl`), the GPU's own `/dev/nvidia<N>` for an
/// aperture (`NV_STATE_PTR(nvlfp->nvptr) != nv`) -- and hold no mapping
/// yet. `instance_of` is the RM instance of the GPU behind a minor.
fn record_mapping(
    target: Option<&dyn INode>,
    mapping: nvctl::MmapContext,
    instance_of: &dyn Fn(u32) -> Option<u32>,
) -> core::result::Result<(), u32> {
    let open = target
        .and_then(|t| t.downcast_ref::<NvidiaOpen>())
        .ok_or(NV_ERR_INVALID_ARGUMENT)?;
    let right_node = match open.node {
        Node::Ctl => mapping.on_ctl(),
        Node::Gpu(minor) => !mapping.on_ctl() && instance_of(minor) == Some(mapping.gpu_instance),
    };
    if !right_node {
        return Err(NV_ERR_INVALID_ARGUMENT);
    }
    let mut state = open.state.lock();
    if state.mapping.is_some() {
        return Err(nvctl::NV_ERR_STATE_IN_USE);
    }
    state.mapping = Some(mapping);
    Ok(())
}

/// `nvidia_mmap_helper`'s checks, and what they leave to map: the physical
/// start, the pages and the caching. Only offset 0 maps (`vm_pgoff != 0` is
/// EINVAL). An aperture maps its exact window and nothing else (ENXIO);
/// system memory maps any length from its first page to the end of the
/// allocation (ERANGE past that).
fn mmap_window(
    mapping: &nvctl::MmapContext,
    offset: usize,
    len: usize,
) -> LxResult<(usize, usize, CachePolicy)> {
    if offset != 0 || len == 0 {
        return Err(LxError::EINVAL);
    }
    let len = pages(len) * PAGE_SIZE;
    match mapping.memory {
        nvctl::MmapMemory::System if len as u64 > mapping.size => return Err(LxError::ERANGE),
        nvctl::MmapMemory::Registers | nvctl::MmapMemory::Framebuffer
            if len as u64 != mapping.size =>
        {
            return Err(LxError::ENXIO)
        }
        _ => {}
    }
    let policy = match mapping.cpu_caching() {
        nvctl::CpuCaching::WriteBack => CachePolicy::Cached,
        nvctl::CpuCaching::WriteCombined => CachePolicy::WriteCombining,
        nvctl::CpuCaching::Uncached => CachePolicy::Uncached,
        nvctl::CpuCaching::UncachedDevice => CachePolicy::UncachedDevice,
    };
    Ok((mapping.start as usize, pages(len), policy))
}

/// One log line the first time an open with no mapping is mmapped: nvidia.ko's
/// "invalid mmap", and the sign of a client that skipped RM_MAP_MEMORY.
fn invalid_mmap() {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        zcore_drivers::klog_warn!(
            "[nvidiactl] mmap of an open with no RM mapping recorded: EINVAL"
        );
    }
}

/// The GPUs CARD_INFO lists, each with its RM id: those the RM has
/// state-loaded.
fn listed_cards() -> Vec<(nvctl::Card, u32)> {
    nvctl::cards()
        .into_iter()
        .filter(|card| card.instance.is_some())
        .map(|card| (card, card.gpu_id()))
        .collect()
}

/// `nvidia_read_card_info`: every whole entry is cleared, then one per
/// listed GPU is filled. Fewer entries than GPUs is EINVAL, after the clear.
fn card_info(payload: &mut [u8], cards: &[(nvctl::Card, u32)]) -> LxResult<()> {
    let entries = payload.len() / CARD_INFO_SIZE;
    payload[..entries * CARD_INFO_SIZE].fill(0);
    if entries < cards.len() {
        return Err(LxError::EINVAL);
    }
    for (i, &(card, gpu_id)) in cards.iter().enumerate() {
        let e = &mut payload[i * CARD_INFO_SIZE..(i + 1) * CARD_INFO_SIZE];
        e[0] = 1; // valid
        put_u32(e, 4, card.domain);
        e[8] = card.bus;
        e[9] = card.slot;
        put_u16(e, 12, PCI_VENDOR_NVIDIA);
        put_u16(e, 14, card.device_id);
        put_u32(e, 16, gpu_id);
        put_u64(e, 24, card.reg_address);
        put_u64(e, 32, card.reg_size);
        put_u64(e, 40, card.fb_address);
        put_u64(e, 48, card.fb_size);
        put_u32(e, 56, card.minor);
    }
    Ok(())
}

/// `RmPerformVersionCheck` over `nv_ioctl_rm_api_version_t`. The reply says
/// the request was recognised before anything is compared; on a mismatch
/// the client's string is logged as nvidia.ko logs it, then replaced with
/// the RM's.
fn check_version(payload: &mut [u8], rm: &[u8]) -> LxResult<()> {
    if payload.len() != RM_API_VERSION_SIZE {
        return Err(LxError::EINVAL);
    }
    put_u32(payload, 4, nvctl::VERSION_REPLY_RECOGNIZED);
    let mut version = [0u8; nvctl::VERSION_STRING_LENGTH];
    version.copy_from_slice(&payload[8..]);
    let ok = nvctl::version_check(u32_at(payload, 0), &mut version, rm);
    if !ok {
        let theirs = version.split(|&b| b == 0).next().unwrap_or(&[]);
        zcore_drivers::klog_warn!(
            "NVRM: API mismatch: the client has the version {}, but this kernel module has the \
             version {}. Please make sure that this kernel module and all NVIDIA driver \
             components have the same version.",
            core::str::from_utf8(theirs).unwrap_or("?"),
            core::str::from_utf8(rm).unwrap_or("?"),
        );
        nvctl::copy_version(&mut version, rm);
    }
    payload[8..].copy_from_slice(&version);
    if ok {
        Ok(())
    } else {
        Err(LxError::EINVAL)
    }
}

/// NV_ESC_SYS_PARAMS: the NUMA memory block size, which the first caller
/// sets and every later one must agree with (EBUSY otherwise).
fn sys_params(payload: &[u8], memblock: &AtomicU64) -> LxResult<()> {
    if payload.len() != SYS_PARAMS_SIZE {
        return Err(LxError::EINVAL);
    }
    let want = u64_at(payload, 0);
    match memblock.compare_exchange(0, want, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => Ok(()),
        Err(have) if have == want => Ok(()),
        Err(_) => Err(LxError::EBUSY),
    }
}

/// NV_ESC_NUMA_INFO: whether the GPU's memory is onlined as a NUMA node of
/// the CPU's, answered as `rm_get_gpu_numa_info` and `nv_get_numa_status`
/// answer for a GPU on PCIe: no node, nothing onlined, NUMA disabled, no
/// automatic onlining, and no retired page inside a region that is empty.
/// Only a GPU on a coherent link (Grace Hopper's) has its memory onlined,
/// and this kernel drives none, so the RM is not asked: its
/// FB_GET_NUMA_INFO gives the same for every GPU here. `memblock` is what
/// the first SYS_PARAMS set, 0 before one.
fn numa_info(payload: &mut [u8], memblock: u64) -> LxResult<()> {
    if payload.len() != NUMA_INFO_SIZE {
        return Err(LxError::EINVAL);
    }
    // Room for more retired pages than the RM can list is its
    // NV_ERR_INVALID_ARGUMENT, which nv.c answers EBUSY.
    if u32_at(payload, NUMA_INFO_OFFLINE_COUNT) > NUMA_MAX_OFFLINE {
        return Err(LxError::EBUSY);
    }
    put_u32(payload, 0, NO_NUMA_NODE as u32);
    put_u32(payload, 4, NUMA_STATUS_DISABLED);
    put_u64(payload, 8, memblock);
    put_u64(payload, 16, 0);
    put_u64(payload, 24, 0);
    payload[32] = 0;
    put_u32(payload, NUMA_INFO_OFFLINE_COUNT, 0);
    Ok(())
}

/// NV_ESC_STATUS_CODE: the adapter status of the GPU at `domain:bus:slot`,
/// NV_OK once the RM has it up. A location with no GPU is EINVAL.
fn status_code(payload: &mut [u8], cards: &[nvctl::Card]) -> LxResult<()> {
    let (domain, bus, slot) = (u32_at(payload, 0), payload[4], payload[5]);
    let card = cards
        .iter()
        .find(|c| c.domain == domain && c.bus == bus && c.slot == slot)
        .ok_or(LxError::EINVAL)?;
    let status = if card.instance.is_some() {
        NV_OK
    } else {
        NV_ERR_NOT_READY
    };
    put_u32(payload, 8, status);
    Ok(())
}

/// The escapes [`not_served`] has logged, one bit per `_IOC_NR`.
static NOT_SERVED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// One log line the first time each escape this kernel does not serve is
/// asked for: that line is what says which escape the next batch needs.
/// Every line of this file but the layout check's `error!` goes to dmesg
/// through klog, not `warn!`, which the `LOG=error` a real machine boots at
/// filters out.
fn not_served(nr: u32) {
    let bit = (nr & 0xff) as usize;
    if NOT_SERVED[bit / 64].fetch_or(1 << (bit % 64), Ordering::Relaxed) & (1 << (bit % 64)) == 0 {
        zcore_drivers::klog_warn!("[nvidiactl] escape {:#x} is not served yet: EINVAL", nr);
    }
}

/// One log line the first time an RM escape finds no RM: the RM core is built
/// by the first GPU attach, so this is a machine with no NVIDIA GPU or a boot
/// that has not attached one.
fn no_rm_yet() {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        zcore_drivers::klog_warn!(
            "[nvidiactl] no RM core yet (no NVIDIA GPU attached): RM escapes are EINVAL"
        );
    }
}

/// The RM instances of every GPU node among `inodes` (the caller's open
/// files), as the bitmask the RM's `osIsGpuAccessible` reads.
pub fn gpu_mask(inodes: impl IntoIterator<Item = Arc<dyn INode>>) -> u32 {
    let cards = nvctl::cards();
    inodes
        .into_iter()
        .filter_map(|inode| inode.downcast_ref::<NvidiaOpen>()?.gpu_minor())
        .filter_map(|minor| cards.get(minor as usize)?.instance)
        .filter(|&instance| instance < 32)
        .fold(0, |mask, instance| mask | 1 << instance)
}

/// The first payload whose size here disagrees with the RM's own table.
fn sizes_mismatch() -> Option<(&'static str, usize, usize)> {
    SIZES
        .iter()
        .zip(nvctl::LAYOUT.iter())
        .find(|&(&ours, &(_, theirs))| ours != theirs)
        .map(|(&ours, &(name, theirs))| (name, ours, theirs))
}

/// `/dev/nvidiactl` and one `/dev/nvidia<N>` per GPU probed, when
/// `nvidia.nvidiactl` is on and every payload this file lays out by hand has
/// the size the RM's C has for it.
pub fn create_nodes(root: &Arc<DevINode>) {
    if !nvctl::enabled() {
        return;
    }
    if let Some((name, ours, theirs)) = nvctl::layout_mismatch().or_else(sizes_mismatch) {
        error!(
            "[nvidiactl] {} is {} bytes here and {} in the RM: no /dev/nvidiactl",
            name, ours, theirs
        );
        return;
    }
    nvctl::set_user_range_validator(kernel_hal::user::user_range_ok);
    nvctl::set_event_sink(post_event);
    if let Err(e) = root.add("nvidiactl", Arc::new(NvidiaDev::new(Node::Ctl))) {
        zcore_drivers::klog_warn!("[nvidiactl] failed to mknod /dev/nvidiactl: {:?}", e);
        return;
    }
    let cards = nvctl::cards();
    for card in cards.iter().filter(|card| card.minor <= GPU_MINOR_MAX) {
        let name = format!("nvidia{}", card.minor);
        if let Err(e) = root.add(&name, Arc::new(NvidiaDev::new(Node::Gpu(card.minor)))) {
            zcore_drivers::klog_warn!("[nvidiactl] failed to mknod /dev/{}: {:?}", name, e);
        }
    }
    let ready = cards.iter().filter(|card| card.instance.is_some()).count();
    zcore_drivers::klog_warn!(
        "[nvidiactl] /dev/nvidiactl and {} GPU node(s), {} with the RM up (RM {})",
        cards.len(),
        ready,
        core::str::from_utf8(nvctl::rm_version()).unwrap_or("?"),
    );
}

mod pin;
pub use pin::pin_user_pages;

#[cfg(test)]
mod tests;
