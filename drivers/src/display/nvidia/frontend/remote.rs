//! `/dev/nvidiactl` and `/dev/nvidia<N>` with the RM in a process
//! (`udrv-nvidia`): the kernel keeps the device files, as it keeps the
//! nv.c half of them (`linux_object::fs::devfs::nvidia`), and what it asks
//! of the RM goes over a link to the process, whose RM answers it as the
//! kernel's would ([`answer`]).
//!
//! What the RM reads and writes of the caller's memory (the parameters an
//! escape points to, as deep as they go) the kernel copies for it while
//! the call waits: `nvidia_rm_sys::frontend::set_user_copy` in the
//! process, over the request the call is.
//!
//! The process says what it has in [`hello`] when it starts serving: whether
//! its RM is up and its GPUs, which the kernel keeps for the device files
//! to read without a call. One RM only: there is no serving while the
//! kernel's own is up, and no second server.
//!
//! A mapping the RM grants (`RM_MAP_MEMORY`) is the kernel's to check
//! before it is recorded: an aperture has to lie in a BAR of a GPU the
//! process holds, system memory in what it pinned (the loader says which,
//! [`Owns`]), or the RM is told it was not recorded.
//!
//! An escape that lends the RM pages of the caller's (an OS descriptor:
//! memory libcuda allocated itself) lends them to the process: the kernel
//! pinned them, and the loader makes them reachable to the process's GPUs
//! ([`Lender`], its IOMMU grants) before the process gets their physical
//! addresses ([`op::RM_ESCAPE_LENDING`]). They are the process's until its
//! RM lets them go (`os_unlock_user_pages`, or the call ending with them
//! untaken), which it says with `ECLIPSE_NVCTL_RETURN` ([`give_back`]). A
//! process that ends with pages it was lent leaves the grants where they
//! are, quarantined with its devices until those are reset.
//!
//! OS events are the process's RM's registrations ([`op::ALLOC_OS_EVENT`]),
//! and what it posts to them it hands the kernel with
//! `ECLIPSE_NVCTL_EVENT`, which queues it on the open as the kernel's own
//! RM's posts are. The nonstall poll that wakes a sleeping CUDA wait asks
//! the process to look ([`op::SERVICE_NONSTALL`]).
//!
//! What `/dev/nvidia-uvm` asks of the RM goes over the link too (the ops
//! from [`op::UVM_VA_SPACE_DEVICE`]): a managed range's pages are lent as an
//! OS descriptor's are, and a semaphore pool's (the kernel's own,
//! [`KernelPages`]) the same way, held by the process's RM for as long as
//! it has memory made of them.
//!
//! A digital stream starting on the HDMI/DP audio function of a served GPU
//! asks the process to push the ELD and unmute ([`op::KICK_HDMI_AUDIO`]),
//! as the kernel's driver does on its own GPUs.
//!
//! Everything the device files ask of the RM is carried.

use super::{
    Caller, Card, ExternalMapping, MmapContext, MmapMemory, NonstallPoll, P2pCaps, Pages,
    RawMmapContext, SkedRange, UserPages, CALLER_NAME_LEN, GPU_UUID_LEN,
};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicU64, Ordering};
use lock::Mutex;
use nvidia_rm_sys::types::{
    NV_ERR_INSUFFICIENT_RESOURCES, NV_ERR_INVALID_STATE, NV_ERR_NOT_SUPPORTED,
    NV_ERR_OPERATING_SYSTEM, NV_STATUS,
};

/// The link's operations, numbered apart from a served node's (`1..=16`)
/// and a served DRM card's (`64..`).
pub mod op {
    /// `a` = the escape's number; the message is the [`Caller`]
    /// ([`super::CALLER_LEN`] bytes) and then the payload. The answer's
    /// `a` is the RM's status, its bytes the payload as the RM left it.
    pub const RM_ESCAPE: u32 = 128;
    /// `a` = the `os_info` of an open that is gone. Told, not answered.
    pub const RELEASE: u32 = 129;
    /// `RM_MAP_MEMORY`: as [`RM_ESCAPE`], and the answer's bytes are the
    /// payload and then the mapping the RM granted
    /// ([`super::MAPPING_LEN`] bytes, `valid` 0 for none).
    pub const RM_MAP_MEMORY: u32 = 130;
    /// The kernel did not record the mapping: `a` = why; the message and
    /// the answer as [`RM_ESCAPE`]'s.
    pub const MAP_UNRECORDED: u32 = 131;
    /// An escape with pages lent: as [`RM_ESCAPE`], with the loan
    /// ([`super::LOAN_LEN`] bytes, then each page's physical address)
    /// between the caller and the payload. The answer as [`RM_ESCAPE`]'s.
    pub const RM_ESCAPE_LENDING: u32 = 132;
    /// `ALLOC_OS_EVENT`: `a` = the client's fd; the message is the client
    /// (`u32`), 4 bytes at zero and the open's `os_info` (`u64`). The
    /// answer's `a` is the RM's status.
    pub const ALLOC_OS_EVENT: u32 = 133;
    /// `FREE_OS_EVENT`: as [`ALLOC_OS_EVENT`], `os_info` at zero.
    pub const FREE_OS_EVENT: u32 = 134;
    /// Look at the nonstall interrupts pending: no message, and the answer's
    /// `a` is what was found ([`super::encode_nonstall`]).
    pub const SERVICE_NONSTALL: u32 = 135;
    /// UVM's calls, each with the [`Caller`] first. `UVM_VA_SPACE_DEVICE`:
    /// then the client, the VA space and the GPU instance (`u32` each, and
    /// 4 bytes at zero); the answer's `a` is a result
    /// ([`super::encode_result`]), the device's handle.
    pub const UVM_VA_SPACE_DEVICE: u32 = 136;
    /// Then the mapping ([`super::EXTERNAL_LEN`] bytes); a result, the
    /// virtual object's handle.
    pub const UVM_MAP_EXTERNAL: u32 = 137;
    /// Then the client and the object (`u32` each); `a` the status.
    pub const UVM_FREE: u32 = 138;
    /// Then the range ([`super::SKED_LEN`] bytes); `a` the status, the
    /// answer's bytes the memory's and the virtual object's handles.
    pub const UVM_SKED_REFLECTED: u32 = 139;
    /// No caller and no message: `a` = the two GPU instances, the first in
    /// the high half. `a` the status, the bytes the caps ([`super::CAPS_LEN`]).
    pub const UVM_P2P_CAPS: u32 = 140;
    /// A semaphore pool's pages, lent: as [`RM_ESCAPE_LENDING`], with the
    /// client, the device, whether the GPU caches them (`u32` each, and 4
    /// bytes at zero) as the payload; a result, the memory's handle.
    pub const UVM_PAGES: u32 = 141;
    /// A managed range's pages, lent: as [`UVM_PAGES`].
    pub const UVM_USER_PAGES: u32 = 142;
    /// A digital stream is starting on an NVIDIA HDMI/DP audio function:
    /// push the ELD and unmute on the GPU that drives the monitor. No
    /// message; the answer's bytes are what was done, in words.
    pub const KICK_HDMI_AUDIO: u32 = 143;
}

/// The most bytes a message carries: a slot of the link.
pub const MESSAGE_MAX: usize = 64 * 1024;

/// `EINVAL`, `ENOSYS`: what a message that is not one is answered with.
pub const EINVAL: i32 = 22;
pub const ENOSYS: i32 = 38;

/// The kernel's end of the link.
pub trait Link: Send + Sync {
    /// Ask, and wait for the answer: its `a` and its bytes, or an errno
    /// (the server's, or `ENODEV` for one that is gone).
    fn call(&self, op: u32, a: u64, data: &[u8]) -> Result<(u64, Vec<u8>), i32>;
    /// Say, and wait for nothing.
    fn tell(&self, op: u32, a: u64);
}

/// A GPU as the process's RM has it: the card, and what its RM calls it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServedCard {
    pub card: Card,
    pub gpu_id: u32,
    pub uuid: Option<[u8; GPU_UUID_LEN]>,
}

/// Whose `len` bytes of physical memory at `start` are, as the loader
/// knows of the process: `Some(true)` for a BAR of a device it holds,
/// `Some(false)` for memory it pinned, `None` for neither.
pub type Owns = Arc<dyn Fn(u64, u64) -> Option<bool> + Send + Sync>;

/// What makes pages the kernel pinned for the RM reachable to the
/// process's GPUs (the IOMMU's grants), the loader's to give. `None` when
/// they cannot be: the escape fails as one that cannot pin.
pub type Lender = Arc<dyn Fn(&UserPages) -> Option<Box<dyn Loan>> + Send + Sync>;

/// What a [`Lender`] did, until it is undone.
pub trait Loan: Send {
    /// The process's RM is done with the pages: its GPUs lose them.
    fn give_back(self: Box<Self>);
}

/// What the kernel knows of the process serving the RM, besides its link.
pub struct Server {
    pub owns: Owns,
    pub lend: Lender,
    /// The process, as [`give_back`] is told who asks.
    pub id: u64,
}

pub(super) struct Served {
    link: Arc<dyn Link>,
    server: Server,
    rm_up: bool,
    cards: Vec<ServedCard>,
    /// Pages lent to the process, by loan, until it gives them back. Those
    /// it never gives back are dropped with it ungiven: what the GPUs were
    /// granted stays theirs until the loader resets them.
    loans: Mutex<BTreeMap<u64, (UserPages, Box<dyn Loan>)>>,
    next_loan: AtomicU64,
}

static SERVED: Mutex<Option<Arc<Served>>> = Mutex::new(None);

/// The process serving the RM, if one is. Cloned out: nothing is called
/// under the lock.
pub(super) fn served() -> Option<Arc<Served>> {
    SERVED.lock().clone()
}

/// Why [`serve`] said no.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The kernel's own RM is up, or another process serves one.
    Taken,
    /// A `hello` that does not parse.
    Invalid,
}

/// Take the RM of the process at the other end of `link`, which says what
/// it has in `hello`. Its GPUs are the cards from then on.
pub fn serve(
    link: Arc<dyn Link>,
    hello: &[u8],
    server: Server,
) -> Result<Vec<ServedCard>, Refused> {
    let (rm_up, cards) = parse_hello(hello).ok_or(Refused::Invalid)?;
    if super::local_rm_up() {
        return Err(Refused::Taken);
    }
    let mut served = SERVED.lock();
    if served.is_some() {
        return Err(Refused::Taken);
    }
    *served = Some(Arc::new(Served {
        link,
        server,
        rm_up,
        cards: cards.clone(),
        loans: Mutex::new(BTreeMap::new()),
        next_loan: AtomicU64::new(1),
    }));
    Ok(cards)
}

/// Whether the process `server` is the one serving the RM: who may post
/// its OS events (`ECLIPSE_NVCTL_EVENT`).
pub fn is_server(server: u64) -> bool {
    served().is_some_and(|s| s.server.id == server)
}

/// `ECLIPSE_NVCTL_RETURN`: the process `server` gives back the pages of
/// `loan`. `false` for a loan it does not have, or a process that does not
/// serve the RM.
pub fn give_back(server: u64, loan: u64) -> bool {
    let Some(served) = served() else {
        return false;
    };
    if served.server.id != server {
        return false;
    }
    let lent = served.loans.lock().remove(&loan);
    match lent {
        Some((pages, loan)) => {
            // The GPUs lose the pages before the kernel unpins them.
            loan.give_back();
            drop(pages);
            true
        }
        None => false,
    }
}

/// The process behind `link` is gone, and its RM with it: the device files
/// find no RM and no GPUs again.
pub fn gone(link: &Arc<dyn Link>) {
    let mut served = SERVED.lock();
    let was = match served.as_ref() {
        Some(s) if Arc::ptr_eq(&s.link, link) => served.take(),
        _ => None,
    };
    drop(served);
    // What it was lent is unpinned (its loans are dropped ungiven) with
    // the lock let go.
    drop(was);
}

impl Served {
    pub(super) fn rm_up(&self) -> bool {
        self.rm_up
    }

    pub(super) fn cards(&self) -> Vec<Card> {
        self.cards.iter().map(|c| c.card).collect()
    }

    /// The card whose RM instance is `instance`.
    pub(super) fn by_instance(&self, instance: u32) -> Option<&ServedCard> {
        self.cards
            .iter()
            .find(|c| c.card.instance == Some(instance))
    }

    pub(super) fn rm_escape(&self, cmd: u32, payload: &mut [u8], caller: &Caller) -> NV_STATUS {
        let msg = with_caller(caller, payload);
        match self.link.call(op::RM_ESCAPE, cmd as u64, &msg) {
            Ok((status, back)) if back.len() == payload.len() => {
                payload.copy_from_slice(&back);
                status as NV_STATUS
            }
            Ok(_) => {
                warn!("[nvidiactl] escape {cmd:#x}: the RM's process answered another size");
                NV_ERR_OPERATING_SYSTEM
            }
            Err(errno) => {
                warn!("[nvidiactl] escape {cmd:#x}: the RM's process did not answer ({errno})");
                NV_ERR_OPERATING_SYSTEM
            }
        }
    }

    pub(super) fn release(&self, os_info: usize) {
        self.link.tell(op::RELEASE, os_info as u64);
    }

    pub(super) fn alloc_os_event(&self, client: u32, os_info: usize, fd: u32) -> NV_STATUS {
        self.os_event(op::ALLOC_OS_EVENT, client, os_info, fd)
    }

    pub(super) fn free_os_event(&self, client: u32, fd: u32) -> NV_STATUS {
        self.os_event(op::FREE_OS_EVENT, client, 0, fd)
    }

    fn os_event(&self, op: u32, client: u32, os_info: usize, fd: u32) -> NV_STATUS {
        match self
            .link
            .call(op, fd as u64, &encode_os_event(client, os_info))
        {
            Ok((status, _)) => status as NV_STATUS,
            Err(errno) => {
                warn!("[nvidiactl] OS event op {op}: the RM's process did not answer ({errno})");
                NV_ERR_OPERATING_SYSTEM
            }
        }
    }

    /// Ask the process to enable HDMI/DP audio on its GPU that drives the
    /// monitor: what it did, or why the process did not say.
    pub(super) fn kick_hdmi_audio(&self) -> String {
        match self.link.call(op::KICK_HDMI_AUDIO, 0, &[]) {
            Ok((_, said)) => String::from_utf8_lossy(&said).into_owned(),
            Err(errno) => alloc::format!(
                "[hdmi-audio] the RM's process (udrv-nvidia) did not answer ({errno})"
            ),
        }
    }

    pub(super) fn service_nonstall(&self) -> NonstallPoll {
        let failed = NonstallPoll::Failed(NV_ERR_OPERATING_SYSTEM);
        match self.link.call(op::SERVICE_NONSTALL, 0, &[]) {
            Ok((word, _)) => decode_nonstall(word).unwrap_or(failed),
            Err(_) => failed,
        }
    }

    /// [`Self::rm_escape`], with `pages` lent to the process's RM.
    pub(super) fn rm_escape_lending(
        &self,
        cmd: u32,
        payload: &mut [u8],
        caller: &Caller,
        pages: UserPages,
    ) -> NV_STATUS {
        match self.lend(op::RM_ESCAPE_LENDING, cmd as u64, caller, pages, payload) {
            Ok((status, back)) if back.len() == payload.len() => {
                payload.copy_from_slice(&back);
                status as NV_STATUS
            }
            Ok(_) => {
                warn!("[nvidiactl] escape {cmd:#x}: the RM's process answered another size");
                NV_ERR_OPERATING_SYSTEM
            }
            Err(status) => status,
        }
    }

    /// Ask `op` with `a`, `pages` lent to the process for it: the caller,
    /// the loan, and `tail` after it. `Err` is the status the call fails
    /// with.
    fn lend(
        &self,
        op: u32,
        a: u64,
        caller: &Caller,
        pages: UserPages,
        tail: &[u8],
    ) -> Result<(u64, Vec<u8>), NV_STATUS> {
        let len = CALLER_LEN + LOAN_LEN + pages.phys.len() * 8 + tail.len();
        if len > MESSAGE_MAX {
            warn!(
                "[nvidiactl] op {op}: {} pages are more than the RM's process can be lent at once",
                pages.phys.len()
            );
            return Err(NV_ERR_INSUFFICIENT_RESOURCES);
        }
        let Some(loan) = (self.server.lend)(&pages) else {
            warn!("[nvidiactl] op {op}: the pages could not be lent to the RM's GPUs");
            return Err(NV_ERR_INSUFFICIENT_RESOURCES);
        };
        let id = self.next_loan.fetch_add(1, Ordering::Relaxed);
        let mut msg = Vec::with_capacity(len);
        msg.extend_from_slice(&encode_caller(caller));
        msg.extend_from_slice(&encode_loan(id, &pages));
        for phys in &pages.phys {
            msg.extend_from_slice(&phys.to_le_bytes());
        }
        msg.extend_from_slice(tail);
        self.loans.lock().insert(id, (pages, loan));
        self.link.call(op, a, &msg).map_err(|errno| {
            warn!("[nvidiactl] op {op}: the RM's process did not answer ({errno})");
            // A message it refused it never made pages of, and gives
            // nothing back: the loan is undone here. One it may have been
            // in the middle of (it is gone) stays, as all of a gone
            // process's do.
            if errno != ENODEV {
                if let Some((pages, loan)) = self.loans.lock().remove(&id) {
                    loan.give_back();
                    drop(pages);
                }
            }
            NV_ERR_OPERATING_SYSTEM
        })
    }

    /// Ask UVM's `op` with `msg` after the caller: the answer's `a` and
    /// bytes, or the status the call fails with.
    fn uvm_call(
        &self,
        op: u32,
        a: u64,
        caller: Option<&Caller>,
        msg: &[u8],
    ) -> Result<(u64, Vec<u8>), NV_STATUS> {
        let msg = match caller {
            Some(caller) => with_caller(caller, msg),
            None => msg.to_vec(),
        };
        self.link.call(op, a, &msg).map_err(|errno| {
            warn!("[nvidiactl] UVM op {op}: the RM's process did not answer ({errno})");
            NV_ERR_OPERATING_SYSTEM
        })
    }

    /// A call whose answer's `a` is a result, and no bytes.
    fn uvm_result(&self, op: u32, caller: &Caller, msg: &[u8]) -> Result<u32, NV_STATUS> {
        let (word, _) = self.uvm_call(op, 0, Some(caller), msg)?;
        decode_result(word).unwrap_or(Err(NV_ERR_OPERATING_SYSTEM))
    }

    pub(super) fn uvm_va_space_device(
        &self,
        caller: &Caller,
        h_client: u32,
        h_va_space: u32,
        gpu_instance: u32,
    ) -> Result<u32, NV_STATUS> {
        let msg = words(&[h_client, h_va_space, gpu_instance, 0]);
        self.uvm_result(op::UVM_VA_SPACE_DEVICE, caller, &msg)
    }

    pub(super) fn uvm_map_external(
        &self,
        caller: &Caller,
        m: &ExternalMapping,
    ) -> Result<u32, NV_STATUS> {
        self.uvm_result(op::UVM_MAP_EXTERNAL, caller, &encode_external(m))
    }

    pub(super) fn uvm_free(&self, caller: &Caller, h_client: u32, h_object: u32) -> NV_STATUS {
        match self.uvm_call(op::UVM_FREE, 0, Some(caller), &words(&[h_client, h_object])) {
            Ok((status, _)) => status as NV_STATUS,
            Err(status) => status,
        }
    }

    pub(super) fn uvm_sked_reflected(
        &self,
        caller: &Caller,
        r: &SkedRange,
    ) -> Result<(u32, u32), NV_STATUS> {
        let (status, back) =
            self.uvm_call(op::UVM_SKED_REFLECTED, 0, Some(caller), &encode_sked(r))?;
        match (status as NV_STATUS, back.len()) {
            (0, 8) => Ok((u32_at(&back, 0), u32_at(&back, 4))),
            (0, _) => Err(NV_ERR_OPERATING_SYSTEM),
            (status, _) => Err(status),
        }
    }

    pub(super) fn uvm_p2p_caps(&self, a: u32, b: u32) -> Result<P2pCaps, NV_STATUS> {
        let pair = u64::from(a) << 32 | u64::from(b);
        let (status, back) = self.uvm_call(op::UVM_P2P_CAPS, pair, None, &[])?;
        match (status as NV_STATUS, back.len()) {
            (0, CAPS_LEN) => Ok(decode_caps(&back)),
            (0, _) => Err(NV_ERR_OPERATING_SYSTEM),
            (status, _) => Err(status),
        }
    }

    pub(super) fn uvm_pages(
        &self,
        caller: &Caller,
        h_client: u32,
        h_device: u32,
        pages: &Pages,
        gpu_cached: bool,
    ) -> Result<u32, NV_STATUS> {
        if pages.size == 0 || !pages.size.is_multiple_of(4096) || !pages.phys.is_multiple_of(4096) {
            return Err(nvidia_rm_sys::types::NV_ERR_INVALID_ARGUMENT);
        }
        let phys = (0..pages.size / 4096)
            .map(|i| pages.phys + i * 4096)
            .collect();
        let lent = UserPages {
            va: 0,
            phys,
            writable: true,
            keep: Box::new(KernelPages(pages.keep.clone())),
        };
        let tail = words(&[h_client, h_device, gpu_cached as u32, 0]);
        let (word, _) = self.lend(op::UVM_PAGES, 0, caller, lent, &tail)?;
        decode_result(word).unwrap_or(Err(NV_ERR_OPERATING_SYSTEM))
    }

    pub(super) fn uvm_user_pages(
        &self,
        caller: &Caller,
        h_client: u32,
        h_device: u32,
        pages: UserPages,
        gpu_cached: bool,
    ) -> Result<u32, NV_STATUS> {
        let tail = words(&[h_client, h_device, gpu_cached as u32, 0]);
        let (word, _) = self.lend(op::UVM_USER_PAGES, 0, caller, pages, &tail)?;
        decode_result(word).unwrap_or(Err(NV_ERR_OPERATING_SYSTEM))
    }

    pub(super) fn rm_map_memory(
        &self,
        payload: &mut [u8],
        caller: &Caller,
    ) -> (NV_STATUS, Option<MmapContext>) {
        let msg = with_caller(caller, payload);
        let (status, back) = match self.link.call(op::RM_MAP_MEMORY, 0, &msg) {
            Ok(answer) => answer,
            Err(errno) => {
                warn!("[nvidiactl] RM_MAP_MEMORY: the RM's process did not answer ({errno})");
                return (NV_ERR_OPERATING_SYSTEM, None);
            }
        };
        if back.len() != payload.len() + MAPPING_LEN {
            warn!("[nvidiactl] RM_MAP_MEMORY: the RM's process answered another size");
            return (NV_ERR_OPERATING_SYSTEM, None);
        }
        payload.copy_from_slice(&back[..payload.len()]);
        let raw = decode_mapping(&back[payload.len()..]);
        let mapping = MmapContext::from_raw(&raw);
        match mapping {
            None if raw.valid != 0 => {
                warn!("[nvidiactl] RM_MAP_MEMORY: a mapping that is not one ({raw:x?})");
                self.map_unrecorded(payload, caller, NV_ERR_INVALID_STATE);
                (NV_ERR_INVALID_STATE, None)
            }
            Some(m) if !self.may_map(&m) => {
                warn!(
                    "[nvidiactl] RM_MAP_MEMORY: {:?} {:#x}+{:#x} is not the RM's process's",
                    m.memory, m.start, m.size
                );
                self.map_unrecorded(payload, caller, NV_ERR_INVALID_STATE);
                (NV_ERR_INVALID_STATE, None)
            }
            mapping => (status as NV_STATUS, mapping),
        }
    }

    /// An aperture in a BAR of a GPU the process holds, system memory in
    /// what it pinned.
    fn may_map(&self, m: &MmapContext) -> bool {
        let device = m.memory != MmapMemory::System;
        (self.server.owns)(m.start, m.size) == Some(device)
    }

    pub(super) fn map_unrecorded(&self, payload: &mut [u8], caller: &Caller, why: NV_STATUS) {
        let msg = with_caller(caller, payload);
        match self.link.call(op::MAP_UNRECORDED, why as u64, &msg) {
            Ok((_, back)) if back.len() == payload.len() => payload.copy_from_slice(&back),
            Ok(_) => warn!("[nvidiactl] MAP_UNRECORDED: the RM's process answered another size"),
            Err(errno) => {
                warn!("[nvidiactl] MAP_UNRECORDED: the RM's process did not answer ({errno})")
            }
        }
    }
}

fn with_caller(caller: &Caller, payload: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(CALLER_LEN + payload.len());
    msg.extend_from_slice(&encode_caller(caller));
    msg.extend_from_slice(payload);
    msg
}

/// A semaphore pool's pages, as they are lent to the process: what keeps
/// them allocated (the kernel's `nvctl::Pages::keep`), for the loader to
/// find the memory they are.
pub struct KernelPages(pub Arc<dyn Any + Send + Sync>);

fn words(w: &[u32]) -> Vec<u8> {
    w.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A UVM call's result as a word: the handle, or the status in the low
/// half with 1 in the high.
pub fn encode_result(r: Result<u32, NV_STATUS>) -> u64 {
    match r {
        Ok(handle) => u64::from(handle),
        Err(status) => 1 << 32 | u64::from(status),
    }
}

pub fn decode_result(word: u64) -> Option<Result<u32, NV_STATUS>> {
    match word >> 32 {
        0 => Some(Ok(word as u32)),
        1 => Some(Err(word as u32)),
        _ => None,
    }
}

/// An `ExternalMapping` on the link: the four handles, offset, base,
/// length, then `read_only` (`u32`) and 4 bytes at zero.
pub const EXTERNAL_LEN: usize = 48;

fn encode_external(m: &ExternalMapping) -> Vec<u8> {
    let mut b = words(&[m.h_client, m.h_device, m.h_va_space, m.h_memory]);
    for w in [m.offset, m.base, m.length] {
        b.extend_from_slice(&w.to_le_bytes());
    }
    b.extend_from_slice(&words(&[m.read_only as u32, 0]));
    b
}

fn decode_external(b: &[u8]) -> Option<ExternalMapping> {
    if b.len() != EXTERNAL_LEN || u32_at(b, 44) != 0 {
        return None;
    }
    Some(ExternalMapping {
        h_client: u32_at(b, 0),
        h_device: u32_at(b, 4),
        h_va_space: u32_at(b, 8),
        h_memory: u32_at(b, 12),
        offset: u64_at(b, 16),
        base: u64_at(b, 24),
        length: u64_at(b, 32),
        read_only: flag(u32_at(b, 40))?,
    })
}

/// A `SkedRange` on the link: the three handles, 4 bytes at zero, base
/// and length.
pub const SKED_LEN: usize = 32;

fn encode_sked(r: &SkedRange) -> Vec<u8> {
    let mut b = words(&[r.h_client, r.h_device, r.h_va_space, 0]);
    b.extend_from_slice(&r.base.to_le_bytes());
    b.extend_from_slice(&r.length.to_le_bytes());
    b
}

fn decode_sked(b: &[u8]) -> Option<SkedRange> {
    if b.len() != SKED_LEN || u32_at(b, 12) != 0 {
        return None;
    }
    Some(SkedRange {
        h_client: u32_at(b, 0),
        h_device: u32_at(b, 4),
        h_va_space: u32_at(b, 8),
        base: u64_at(b, 16),
        length: u64_at(b, 24),
    })
}

/// `P2pCaps` on the link: read, write, atomics, a byte at zero, then the
/// connectivity.
pub const CAPS_LEN: usize = 8;

fn encode_caps(c: &P2pCaps) -> [u8; CAPS_LEN] {
    let mut b = [0u8; CAPS_LEN];
    b[0] = c.read;
    b[1] = c.write;
    b[2] = c.atomics;
    b[4..8].copy_from_slice(&c.connectivity.to_le_bytes());
    b
}

fn decode_caps(b: &[u8]) -> P2pCaps {
    P2pCaps {
        read: b[0],
        write: b[1],
        atomics: b[2],
        connectivity: u32_at(b, 4),
    }
}

fn flag(w: u32) -> Option<bool> {
    match w {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// The client, the device and whether the GPU caches them, after a loan.
fn decode_pages_tail(b: &[u8]) -> Option<(u32, u32, bool)> {
    if b.len() != 16 || u32_at(b, 12) != 0 {
        return None;
    }
    Some((u32_at(b, 0), u32_at(b, 4), flag(u32_at(b, 8))?))
}

/// An OS event's message: the client, 4 bytes at zero, the open.
const OS_EVENT_LEN: usize = 16;

fn encode_os_event(client: u32, os_info: usize) -> [u8; OS_EVENT_LEN] {
    let mut b = [0u8; OS_EVENT_LEN];
    b[0..4].copy_from_slice(&client.to_le_bytes());
    b[8..16].copy_from_slice(&(os_info as u64).to_le_bytes());
    b
}

fn decode_os_event(b: &[u8]) -> Option<(u32, usize)> {
    if b.len() != OS_EVENT_LEN || u32_at(b, 4) != 0 {
        return None;
    }
    Some((u32_at(b, 0), usize::try_from(u64_at(b, 8)).ok()?))
}

/// A look at the nonstall interrupts as a word: its kind in the high half
/// (0 serviced, 1 busy, 2 failed), the GPUs serviced or the status in the
/// low.
pub fn encode_nonstall(poll: NonstallPoll) -> u64 {
    match poll {
        NonstallPoll::Serviced(gpus) => u64::from(gpus),
        NonstallPoll::Busy => 1 << 32,
        NonstallPoll::Failed(status) => 2 << 32 | u64::from(status),
    }
}

pub fn decode_nonstall(word: u64) -> Option<NonstallPoll> {
    let low = word as u32;
    match word >> 32 {
        0 => Some(NonstallPoll::Serviced(low)),
        1 if low == 0 => Some(NonstallPoll::Busy),
        2 => Some(NonstallPoll::Failed(low)),
        _ => None,
    }
}

/// A loan on the link: its id, the user address of its first page,
/// whether it is writable (`u32`), its page count (`u32`), and 8 bytes
/// kept at zero.
pub const LOAN_LEN: usize = 32;

fn encode_loan(id: u64, pages: &UserPages) -> [u8; LOAN_LEN] {
    let mut b = [0u8; LOAN_LEN];
    b[0..8].copy_from_slice(&id.to_le_bytes());
    b[8..16].copy_from_slice(&(pages.va as u64).to_le_bytes());
    b[16..20].copy_from_slice(&(pages.writable as u32).to_le_bytes());
    b[20..24].copy_from_slice(&(pages.phys.len() as u32).to_le_bytes());
    b
}

/// The pages of the loan at the start of `data`, as the process's RM takes
/// them, and the bytes after them.
fn decode_loan(data: &[u8]) -> Option<(UserPages, &[u8])> {
    let head = data.get(..LOAN_LEN)?;
    if u64_at(head, 24) != 0 {
        return None;
    }
    let writable = match u32_at(head, 16) {
        0 => false,
        1 => true,
        _ => return None,
    };
    let count = u32_at(head, 20) as usize;
    let end = LOAN_LEN.checked_add(count.checked_mul(8)?)?;
    let phys = data
        .get(LOAN_LEN..end)?
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| u64::from_le_bytes(*b))
        .collect();
    let pages = UserPages {
        va: usize::try_from(u64_at(head, 8)).ok()?,
        phys,
        writable,
        keep: Box::new(Lent(u64_at(head, 0))),
    };
    Some((pages, &data[end..]))
}

/// Pages lent to this process, which go back to the kernel when they are
/// dropped: when the RM unlocks them, or did not take them.
struct Lent(u64);

impl Drop for Lent {
    fn drop(&mut self) {
        if let Some(give_back) = *GIVE_BACK.lock() {
            give_back(self.0);
        }
    }
}

static GIVE_BACK: Mutex<Option<fn(u64)>> = Mutex::new(None);

/// How this process gives the kernel back a loan its RM is done with
/// (`ECLIPSE_NVCTL_RETURN`), before it serves.
pub fn set_give_back(give_back: fn(u64)) {
    *GIVE_BACK.lock() = Some(give_back);
}

/// `ENODEV`: the link's server is gone.
const ENODEV: i32 = 19;

/// A mapping on the link: `RawMmapContext`'s words, in its order.
pub const MAPPING_LEN: usize = 40;

fn encode_mapping(m: Option<&MmapContext>) -> [u8; MAPPING_LEN] {
    let raw = m.map(MmapContext::to_raw).unwrap_or_default();
    let mut b = [0u8; MAPPING_LEN];
    let words = [
        raw.valid,
        raw.ctl,
        raw.gpu_instance,
        raw.memory_type,
        raw.caching,
        raw.prot,
    ];
    for (i, w) in words.iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    b[24..32].copy_from_slice(&raw.start.to_le_bytes());
    b[32..40].copy_from_slice(&raw.size.to_le_bytes());
    b
}

fn decode_mapping(b: &[u8]) -> RawMmapContext {
    RawMmapContext {
        valid: u32_at(b, 0),
        ctl: u32_at(b, 4),
        gpu_instance: u32_at(b, 8),
        memory_type: u32_at(b, 12),
        caching: u32_at(b, 16),
        prot: u32_at(b, 20),
        start: u64_at(b, 24),
        size: u64_at(b, 32),
    }
}

/// The RM a process serves, as [`answer`] reaches it.
pub trait Rm {
    fn escape(&self, cmd: u32, payload: &mut [u8], caller: &Caller) -> NV_STATUS;
    fn release(&self, os_info: usize);
    fn escape_lending(
        &self,
        cmd: u32,
        payload: &mut [u8],
        caller: &Caller,
        pages: UserPages,
    ) -> NV_STATUS;
    fn map_memory(&self, payload: &mut [u8], caller: &Caller) -> (NV_STATUS, Option<MmapContext>);
    fn map_unrecorded(&self, payload: &mut [u8], caller: &Caller, why: NV_STATUS);
    fn alloc_os_event(&self, client: u32, os_info: usize, fd: u32) -> NV_STATUS;
    fn free_os_event(&self, client: u32, fd: u32) -> NV_STATUS;
    fn service_nonstall(&self) -> NonstallPoll;
    /// Enable HDMI/DP audio on the GPU that drives the monitor, and say
    /// what was done.
    fn kick_hdmi_audio(&self) -> String {
        String::new()
    }

    // UVM's calls, refused unless an RM takes them.
    fn uvm_va_space_device(
        &self,
        _caller: &Caller,
        _h_client: u32,
        _h_va_space: u32,
        _gpu_instance: u32,
    ) -> Result<u32, NV_STATUS> {
        Err(NV_ERR_NOT_SUPPORTED)
    }
    fn uvm_map_external(&self, _caller: &Caller, _m: &ExternalMapping) -> Result<u32, NV_STATUS> {
        Err(NV_ERR_NOT_SUPPORTED)
    }
    fn uvm_free(&self, _caller: &Caller, _h_client: u32, _h_object: u32) -> NV_STATUS {
        NV_ERR_NOT_SUPPORTED
    }
    fn uvm_sked_reflected(
        &self,
        _caller: &Caller,
        _r: &SkedRange,
    ) -> Result<(u32, u32), NV_STATUS> {
        Err(NV_ERR_NOT_SUPPORTED)
    }
    fn uvm_p2p_caps(&self, _a: u32, _b: u32) -> Result<P2pCaps, NV_STATUS> {
        Err(NV_ERR_NOT_SUPPORTED)
    }
    fn uvm_pages(
        &self,
        _caller: &Caller,
        _h_client: u32,
        _h_device: u32,
        _pages: &Pages,
        _gpu_cached: bool,
    ) -> Result<u32, NV_STATUS> {
        Err(NV_ERR_NOT_SUPPORTED)
    }
    fn uvm_user_pages(
        &self,
        _caller: &Caller,
        _h_client: u32,
        _h_device: u32,
        _pages: UserPages,
        _gpu_cached: bool,
    ) -> Result<u32, NV_STATUS> {
        Err(NV_ERR_NOT_SUPPORTED)
    }
}

/// This address space's RM: the driver's own.
pub struct ThisRm;

impl Rm for ThisRm {
    fn escape(&self, cmd: u32, payload: &mut [u8], caller: &Caller) -> NV_STATUS {
        super::rm_escape(cmd, payload, caller)
    }

    fn release(&self, os_info: usize) {
        super::release(os_info)
    }

    fn escape_lending(
        &self,
        cmd: u32,
        payload: &mut [u8],
        caller: &Caller,
        pages: UserPages,
    ) -> NV_STATUS {
        super::rm_escape_lending(cmd, payload, caller, Some(pages))
    }

    fn map_memory(&self, payload: &mut [u8], caller: &Caller) -> (NV_STATUS, Option<MmapContext>) {
        super::rm_map_memory(payload, caller)
    }

    fn map_unrecorded(&self, payload: &mut [u8], caller: &Caller, why: NV_STATUS) {
        super::map_unrecorded(payload, caller, why)
    }

    fn alloc_os_event(&self, client: u32, os_info: usize, fd: u32) -> NV_STATUS {
        super::alloc_os_event(client, os_info, fd)
    }

    fn free_os_event(&self, client: u32, fd: u32) -> NV_STATUS {
        super::free_os_event(client, fd)
    }

    fn service_nonstall(&self) -> NonstallPoll {
        super::service_nonstall()
    }

    fn kick_hdmi_audio(&self) -> String {
        crate::display::nvidia::NvidiaGpu::kick_hdmi_audio_all();
        let mut said = crate::display::nvidia::hdmi_audio_status();
        let mut end = said.len().min(MESSAGE_MAX);
        while !said.is_char_boundary(end) {
            end -= 1;
        }
        said.truncate(end);
        said
    }

    fn uvm_va_space_device(
        &self,
        caller: &Caller,
        h_client: u32,
        h_va_space: u32,
        gpu_instance: u32,
    ) -> Result<u32, NV_STATUS> {
        super::uvm_va_space_device(caller, h_client, h_va_space, gpu_instance)
    }

    fn uvm_map_external(&self, caller: &Caller, m: &ExternalMapping) -> Result<u32, NV_STATUS> {
        super::uvm_map_external(caller, m)
    }

    fn uvm_free(&self, caller: &Caller, h_client: u32, h_object: u32) -> NV_STATUS {
        super::uvm_free(caller, h_client, h_object)
    }

    fn uvm_sked_reflected(&self, caller: &Caller, r: &SkedRange) -> Result<(u32, u32), NV_STATUS> {
        super::uvm_sked_reflected(caller, r)
    }

    fn uvm_p2p_caps(&self, a: u32, b: u32) -> Result<P2pCaps, NV_STATUS> {
        super::uvm_p2p_caps(a, b)
    }

    fn uvm_pages(
        &self,
        caller: &Caller,
        h_client: u32,
        h_device: u32,
        pages: &Pages,
        gpu_cached: bool,
    ) -> Result<u32, NV_STATUS> {
        super::uvm_pages(caller, h_client, h_device, pages, gpu_cached)
    }

    fn uvm_user_pages(
        &self,
        caller: &Caller,
        h_client: u32,
        h_device: u32,
        pages: UserPages,
        gpu_cached: bool,
    ) -> Result<u32, NV_STATUS> {
        super::uvm_user_pages(caller, h_client, h_device, pages, gpu_cached)
    }
}

/// The server's end: the answer to message `op` with `a` and `data`, its
/// bytes in `out`. `Err` is the errno the kernel is told.
pub fn answer(rm: &dyn Rm, op: u32, a: u64, data: &[u8], out: &mut Vec<u8>) -> Result<u64, i32> {
    let with_payload = |out: &mut Vec<u8>| -> Result<Caller, i32> {
        let caller = data
            .get(..CALLER_LEN)
            .and_then(decode_caller)
            .ok_or(EINVAL)?;
        out.clear();
        out.extend_from_slice(&data[CALLER_LEN..]);
        Ok(caller)
    };
    match op {
        op::RM_ESCAPE => {
            let cmd = u32::try_from(a).map_err(|_| EINVAL)?;
            let caller = with_payload(out)?;
            Ok(rm.escape(cmd, out, &caller) as u64)
        }
        op::RM_ESCAPE_LENDING => {
            let cmd = u32::try_from(a).map_err(|_| EINVAL)?;
            let caller = data
                .get(..CALLER_LEN)
                .and_then(decode_caller)
                .ok_or(EINVAL)?;
            let (pages, payload) = decode_loan(&data[CALLER_LEN..]).ok_or(EINVAL)?;
            out.clear();
            out.extend_from_slice(payload);
            Ok(rm.escape_lending(cmd, out, &caller, pages) as u64)
        }
        op::RM_MAP_MEMORY => {
            let caller = with_payload(out)?;
            let (status, mapping) = rm.map_memory(out, &caller);
            out.extend_from_slice(&encode_mapping(mapping.as_ref()));
            Ok(status as u64)
        }
        op::MAP_UNRECORDED => {
            let why = u32::try_from(a).map_err(|_| EINVAL)?;
            let caller = with_payload(out)?;
            rm.map_unrecorded(out, &caller, why);
            Ok(0)
        }
        op::RELEASE => {
            rm.release(a as usize);
            Ok(0)
        }
        op::ALLOC_OS_EVENT | op::FREE_OS_EVENT => {
            let fd = u32::try_from(a).map_err(|_| EINVAL)?;
            let (client, os_info) = decode_os_event(data).ok_or(EINVAL)?;
            out.clear();
            let status = if op == op::ALLOC_OS_EVENT {
                rm.alloc_os_event(client, os_info, fd)
            } else {
                rm.free_os_event(client, fd)
            };
            Ok(status as u64)
        }
        op::SERVICE_NONSTALL => {
            out.clear();
            Ok(encode_nonstall(rm.service_nonstall()))
        }
        op::KICK_HDMI_AUDIO => {
            out.clear();
            out.extend_from_slice(rm.kick_hdmi_audio().as_bytes());
            Ok(0)
        }
        op::UVM_P2P_CAPS => {
            out.clear();
            match rm.uvm_p2p_caps((a >> 32) as u32, a as u32) {
                Ok(caps) => {
                    out.extend_from_slice(&encode_caps(&caps));
                    Ok(0)
                }
                Err(status) => Ok(status as u64),
            }
        }
        op::UVM_VA_SPACE_DEVICE
        | op::UVM_MAP_EXTERNAL
        | op::UVM_FREE
        | op::UVM_SKED_REFLECTED
        | op::UVM_PAGES
        | op::UVM_USER_PAGES => {
            let caller = data
                .get(..CALLER_LEN)
                .and_then(decode_caller)
                .ok_or(EINVAL)?;
            out.clear();
            answer_uvm(rm, op, &caller, &data[CALLER_LEN..], out)
        }
        _ => Err(ENOSYS),
    }
}

/// [`answer`] to a UVM call of `caller`'s, its message after the caller
/// in `msg`.
fn answer_uvm(
    rm: &dyn Rm,
    op: u32,
    caller: &Caller,
    msg: &[u8],
    out: &mut Vec<u8>,
) -> Result<u64, i32> {
    match op {
        op::UVM_VA_SPACE_DEVICE => {
            if msg.len() != 16 || u32_at(msg, 12) != 0 {
                return Err(EINVAL);
            }
            let r = rm.uvm_va_space_device(caller, u32_at(msg, 0), u32_at(msg, 4), u32_at(msg, 8));
            Ok(encode_result(r))
        }
        op::UVM_MAP_EXTERNAL => {
            let m = decode_external(msg).ok_or(EINVAL)?;
            Ok(encode_result(rm.uvm_map_external(caller, &m)))
        }
        op::UVM_FREE => {
            if msg.len() != 8 {
                return Err(EINVAL);
            }
            Ok(rm.uvm_free(caller, u32_at(msg, 0), u32_at(msg, 4)) as u64)
        }
        op::UVM_SKED_REFLECTED => {
            let r = decode_sked(msg).ok_or(EINVAL)?;
            match rm.uvm_sked_reflected(caller, &r) {
                Ok((memory, virt)) => {
                    out.extend_from_slice(&words(&[memory, virt]));
                    Ok(0)
                }
                Err(status) => Ok(status as u64),
            }
        }
        _ => {
            let (pages, tail) = decode_loan(msg).ok_or(EINVAL)?;
            let (h_client, h_device, cached) = decode_pages_tail(tail).ok_or(EINVAL)?;
            if op == op::UVM_USER_PAGES {
                let r = rm.uvm_user_pages(caller, h_client, h_device, pages, cached);
                return Ok(encode_result(r));
            }
            // The kernel's pages: one block, held for as long as the RM
            // has memory made of them, given back when it lets them go.
            let first = pages.phys.first().copied().ok_or(EINVAL)?;
            let contiguous = pages
                .phys
                .iter()
                .enumerate()
                .all(|(i, &p)| p == first + i as u64 * 4096);
            if !contiguous {
                return Err(EINVAL);
            }
            let size = pages.phys.len() as u64 * 4096;
            let keep: Arc<dyn Any + Send + Sync> = match pages.keep.downcast::<Lent>() {
                Ok(lent) => Arc::new(*lent),
                Err(_) => return Err(EINVAL),
            };
            let block = Pages {
                phys: first,
                size,
                keep: &keep,
            };
            Ok(encode_result(
                rm.uvm_pages(caller, h_client, h_device, &block, cached),
            ))
        }
    }
}

/// What this process's RM says of itself, for `ECLIPSE_NVCTL_SERVE`.
pub fn hello() -> Vec<u8> {
    let cards: Vec<ServedCard> = super::cards()
        .into_iter()
        .map(|card| ServedCard {
            card,
            gpu_id: card.gpu_id(),
            uuid: card.uuid(),
        })
        .collect();
    hello_of(super::rm_up(), &cards)
}

/// `"NVCT"`, then the format's version.
const MAGIC: u32 = 0x5443_564e;
const VERSION: u32 = 1;
const HEAD_LEN: usize = 16;
const CARD_LEN: usize = 80;
/// The most GPUs a `hello` lists.
const CARDS_MAX: usize = 32;

pub fn hello_of(rm_up: bool, cards: &[ServedCard]) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEAD_LEN + cards.len() * CARD_LEN);
    for word in [MAGIC, VERSION, rm_up as u32, cards.len() as u32] {
        b.extend_from_slice(&word.to_le_bytes());
    }
    for c in cards {
        let card = &c.card;
        let at = b.len();
        b.resize(at + CARD_LEN, 0);
        let e = &mut b[at..];
        e[0..4].copy_from_slice(&card.minor.to_le_bytes());
        e[4..8].copy_from_slice(&card.domain.to_le_bytes());
        e[8] = card.bus;
        e[9] = card.slot;
        e[10..12].copy_from_slice(&card.device_id.to_le_bytes());
        e[12..16].copy_from_slice(&(card.instance.is_some() as u32).to_le_bytes());
        e[16..20].copy_from_slice(&card.instance.unwrap_or(0).to_le_bytes());
        e[20..24].copy_from_slice(&c.gpu_id.to_le_bytes());
        e[24..28].copy_from_slice(&(c.uuid.is_some() as u32).to_le_bytes());
        e[32..40].copy_from_slice(&card.reg_address.to_le_bytes());
        e[40..48].copy_from_slice(&card.reg_size.to_le_bytes());
        e[48..56].copy_from_slice(&card.fb_address.to_le_bytes());
        e[56..64].copy_from_slice(&card.fb_size.to_le_bytes());
        e[64..80].copy_from_slice(&c.uuid.unwrap_or([0; GPU_UUID_LEN]));
    }
    b
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from(u32_at(b, at)) | u64::from(u32_at(b, at + 4)) << 32
}

pub fn parse_hello(b: &[u8]) -> Option<(bool, Vec<ServedCard>)> {
    if b.len() < HEAD_LEN || u32_at(b, 0) != MAGIC || u32_at(b, 4) != VERSION {
        return None;
    }
    let rm_up = match u32_at(b, 8) {
        0 => false,
        1 => true,
        _ => return None,
    };
    let count = u32_at(b, 12) as usize;
    if count > CARDS_MAX || b.len() != HEAD_LEN + count * CARD_LEN {
        return None;
    }
    let mut cards = Vec::with_capacity(count);
    for e in b[HEAD_LEN..].chunks(CARD_LEN) {
        let flag = |at| match u32_at(e, at) {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        let mut uuid = [0u8; GPU_UUID_LEN];
        uuid.copy_from_slice(&e[64..80]);
        let card = Card {
            minor: u32_at(e, 0),
            domain: u32_at(e, 4),
            bus: e[8],
            slot: e[9],
            device_id: u16::from_le_bytes([e[10], e[11]]),
            reg_address: u64_at(e, 32),
            reg_size: u64_at(e, 40),
            fb_address: u64_at(e, 48),
            fb_size: u64_at(e, 56),
            instance: flag(12)?.then(|| u32_at(e, 16)),
        };
        // Each card's minor is its place, as `cards()` lists them.
        if card.minor as usize != cards.len() {
            return None;
        }
        cards.push(ServedCard {
            card,
            gpu_id: u32_at(e, 20),
            uuid: flag(24)?.then_some(uuid),
        });
    }
    Some((rm_up, cards))
}

/// A [`Caller`] on the link.
pub const CALLER_LEN: usize = 24 + CALLER_NAME_LEN;

fn encode_caller(c: &Caller) -> [u8; CALLER_LEN] {
    let mut b = [0u8; CALLER_LEN];
    b[0..8].copy_from_slice(&(c.os_info as u64).to_le_bytes());
    b[8] = c.ctl as u8;
    b[9] = c.admin as u8;
    b[12..16].copy_from_slice(&c.gpu_mask.to_le_bytes());
    b[16..20].copy_from_slice(&c.pid.to_le_bytes());
    b[24..].copy_from_slice(&c.name);
    b
}

fn decode_caller(b: &[u8]) -> Option<Caller> {
    let flag = |v: u8| match v {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    let mut name = [0u8; CALLER_NAME_LEN];
    name.copy_from_slice(&b[24..CALLER_LEN]);
    // The RM prints it with `%s`: it always ends.
    name[CALLER_NAME_LEN - 1] = 0;
    Some(Caller {
        os_info: usize::try_from(u64_at(b, 0)).ok()?,
        ctl: flag(b[8])?,
        admin: flag(b[9])?,
        gpu_mask: u32_at(b, 12),
        pid: u32_at(b, 16),
        name,
    })
}

#[cfg(test)]
mod tests;
