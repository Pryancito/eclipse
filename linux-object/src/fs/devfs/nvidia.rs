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
//! Served here: CARD_INFO, CHECK_VERSION_STR, SYS_PARAMS, ATTACH_GPUS_TO_FD,
//! WAIT_OPEN_COMPLETE, STATUS_CODE and REGISTER_FD. Served by the RM
//! (`nvidia-rm-sys/vendor/eclipse_rm_frontend.c`): RM_ALLOC, RM_FREE,
//! RM_CONTROL, RM_DUP_OBJECT and RM_SHARE. Everything else answers EINVAL
//! and is logged the first time each escape is seen: RM_MAP_MEMORY and the
//! `mmap` behind it, the OS events, RM_ALLOC_MEMORY, VID_HEAP_CONTROL, the
//! NUMA and dma-buf escapes. `nvidia-smi` is what this much is for; CUDA
//! needs those, and `/dev/nvidia-uvm` besides.
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

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kernel_hal::user::{UserInPtr, UserOutPtr};
use lock::Mutex;
use rcore_fs::vfs::*;
use rcore_fs_devfs::{DevFS, DevINode};
use zcore_drivers::display::nvctl;

use crate::error::{LxError, LxResult};

/// `NV_MAJOR_DEVICE_NUMBER`.
pub const MAJOR: usize = 195;
/// `NV_MINOR_DEVICE_NUMBER_CONTROL_DEVICE`.
pub const CTL_MINOR: u32 = 255;
/// `NV_MINOR_DEVICE_NUMBER_REGULAR_MAX`: the last minor a GPU can have.
const GPU_MINOR_MAX: u32 = 247;

/// The escapes this file knows by name (nv-ioctl-numbers.h, nv_escape.h).
pub mod esc {
    pub const RM_FREE: u32 = 0x29;
    pub const RM_CONTROL: u32 = 0x2A;
    pub const RM_ALLOC: u32 = 0x2B;
    pub const RM_DUP_OBJECT: u32 = 0x34;
    pub const RM_SHARE: u32 = 0x35;
    pub const CARD_INFO: u32 = 200;
    pub const REGISTER_FD: u32 = 201;
    pub const STATUS_CODE: u32 = 209;
    pub const CHECK_VERSION_STR: u32 = 210;
    pub const IOCTL_XFER_CMD: u32 = 211;
    pub const ATTACH_GPUS_TO_FD: u32 = 212;
    pub const SYS_PARAMS: u32 = 214;
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
const SIZES: [usize; 7] = [
    XFER_SIZE,
    CARD_INFO_SIZE,
    RM_API_VERSION_SIZE,
    SYS_PARAMS_SIZE,
    REGISTER_FD_SIZE,
    WAIT_OPEN_COMPLETE_SIZE,
    STATUS_CODE_SIZE,
];

const NV_OK: u32 = 0;
const NV_ERR_NOT_READY: u32 = 0x55;
const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
const PCI_VENDOR_NVIDIA: u16 = 0x10de;

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
}

#[derive(Default)]
struct OpenState {
    /// ATTACH_GPUS_TO_FD's list (`nvlfp->attached_gpus`), set at most once.
    attached_gpus: Option<Vec<u32>>,
    /// REGISTER_FD's control open (`nvfp->ctl_nvfp`), held for as long as
    /// this open lives, as Linux holds `ctl_nvfp_priv`.
    ctl: Option<Arc<dyn INode>>,
}

impl NvidiaOpen {
    fn new(node: Node, inode_id: usize) -> Self {
        NvidiaOpen {
            node,
            inode_id,
            state: Mutex::new(OpenState::default()),
        }
    }

    fn os_info(&self) -> usize {
        self as *const Self as usize
    }

    fn is_ctl(&self) -> bool {
        self.node == Node::Ctl
    }

    /// `NV_CTL_DEVICE_ONLY`.
    fn ctl_only(&self) -> LxResult<()> {
        if self.is_ctl() {
            Ok(())
        } else {
            Err(LxError::EINVAL)
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
            esc::RM_ALLOC | esc::RM_FREE | esc::RM_CONTROL | esc::RM_DUP_OBJECT | esc::RM_SHARE => {
                if !nvctl::rm_up() {
                    no_rm_yet();
                    return Err(LxError::EINVAL);
                }
                let rm_caller = nvctl::Caller {
                    os_info: self.os_info(),
                    ctl: self.is_ctl(),
                    admin: caller.admin,
                    gpu_mask: (caller.gpu_mask)(),
                };
                match nvctl::rm_escape(nr, payload, &rm_caller) {
                    NV_OK => Ok(()),
                    NV_ERR_NOT_SUPPORTED => {
                        not_served(nr);
                        Err(LxError::EINVAL)
                    }
                    _ => Err(LxError::EINVAL),
                }
            }
            _ => {
                not_served(nr);
                Err(LxError::EINVAL)
            }
        }
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

impl Drop for NvidiaOpen {
    fn drop(&mut self) {
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

    /// No OS events yet, so nothing ever becomes readable.
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus::default())
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
        warn!(
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

/// One log line the first time each escape this kernel does not serve is
/// asked for: that line is what says which escape the next batch needs.
fn not_served(nr: u32) {
    static SEEN: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    let bit = (nr & 0xff) as usize;
    if SEEN[bit / 64].fetch_or(1 << (bit % 64), Ordering::Relaxed) & (1 << (bit % 64)) == 0 {
        warn!("[nvidiactl] escape {:#x} is not served yet: EINVAL", nr);
    }
}

/// One log line the first time an RM escape finds no RM: the RM core is built
/// by the first GPU attach, so this is a machine with no NVIDIA GPU or a boot
/// that has not attached one.
fn no_rm_yet() {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        warn!("[nvidiactl] no RM core yet (no NVIDIA GPU attached): RM escapes are EINVAL");
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
    if let Err(e) = root.add("nvidiactl", Arc::new(NvidiaDev::new(Node::Ctl))) {
        warn!("[nvidiactl] failed to mknod /dev/nvidiactl: {:?}", e);
        return;
    }
    let cards = nvctl::cards();
    for card in cards.iter().filter(|card| card.minor <= GPU_MINOR_MAX) {
        let name = format!("nvidia{}", card.minor);
        if let Err(e) = root.add(&name, Arc::new(NvidiaDev::new(Node::Gpu(card.minor)))) {
            warn!("[nvidiactl] failed to mknod /dev/{}: {:?}", name, e);
        }
    }
    let ready = cards.iter().filter(|card| card.instance.is_some()).count();
    warn!(
        "[nvidiactl] /dev/nvidiactl and {} GPU node(s), {} with the RM up (RM {})",
        cards.len(),
        ready,
        core::str::from_utf8(nvctl::rm_version()).unwrap_or("?"),
    );
}

#[cfg(test)]
mod tests;
