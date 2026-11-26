//! A DRM card whose driver is a process (`udrv-nvidia`): the DRM core
//! sees one more card, and each call it makes of it goes over the same
//! link a served device node's requests do.
//!
//! The format and both of its ends are in `zcore-drivers`
//! (`scheme::drm_remote`): this is only the link under it, and the card's
//! place in the DRM core's list and in `/dev/dri`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rcore_fs::vfs::INode;
use zcore_drivers::scheme::drm_remote::{self, Fence, Remote, Transport};
use zcore_drivers::scheme::DrmScheme;
use zircon_object::ipc::Fifo;
use zircon_object::object::KernelObject;
use zircon_object::vm::VmObject;

use super::link::{Fail, Kill, Link};
use crate::error::{LxError, LxResult};
use crate::fs::devfs::drm;
use crate::process::LinuxProcess;

/// Where the kernel reads a fence of the driver's, and the ctx its table
/// knows the channel by, if the landing zone is the driver's own memory
/// (`Transport::fence`): the gateway that served the card knows what the
/// driver holds.
pub type FenceCheck = Arc<dyn Fn(&Fence) -> Option<(usize, u32)> + Send + Sync>;

/// The link, as the card's transport. Each answer carries the card's
/// flags in its `fh`, kept here so the DRM core can read them under its
/// lock without a call (a call waits, and must not wait holding a lock).
pub struct OverLink(Arc<Link>, AtomicU64, Option<FenceCheck>);

/// How long a console GPU's bring-up may keep its driver silent: the
/// kernel's own waits up to the scanout pause's 90 s watchdog, and a GSP
/// boot that never comes back is still a hang past this.
const BRINGUP_PATIENCE_MS: u64 = 180_000;

/// No answer has carried flags yet.
const NO_FLAGS: u64 = u64::MAX;

impl OverLink {
    pub(super) fn new(link: Arc<Link>, fence: Option<FenceCheck>) -> Self {
        Self(link, AtomicU64::new(NO_FLAGS), fence)
    }
}

impl Transport for OverLink {
    fn call(&self, op: u32, a: u64, b: u64, data: &[u8]) -> Result<(u64, Vec<u8>), i32> {
        // A console GPU's bring-up boots its GSP firmware, which takes longer
        // than the link waits before it calls the driver hung: killing the
        // driver halfway through that boot left the monitor black.
        if matches!(
            op,
            drm_remote::op::CONSOLE_BRINGUP | drm_remote::op::CONSOLE_GR_LADDER
        ) {
            self.0.be_patient_for(BRINGUP_PATIENCE_MS);
        }
        // The message's `len` is the bytes it carries; the answer has the
        // whole slot.
        match self.0.call(op, 0, 0, a, b, data) {
            Ok(answer) => {
                self.1.store(answer.fh as u64, Ordering::Relaxed);
                let mut bytes = vec![0u8; answer.len];
                answer.data(&mut bytes).map_err(|_| drm_remote::EINVAL)?;
                Ok((answer.a, bytes))
            }
            Err(Fail::Said(errno)) => Err(errno as i32),
            Err(Fail::Gone) => Err(drm_remote::ENODEV),
        }
    }

    fn tell(&self, op: u32, a: u64, b: u64) {
        self.0.tell_with(op, 0, a, b);
    }

    fn read_user(&self, addr: usize, len: usize) -> Result<Vec<u8>, i32> {
        kernel_hal::user::UserInPtr::<u8>::from(addr)
            .read_array(len)
            .map_err(|_| drm_remote::ioctl::EFAULT)
    }

    fn write_user(&self, addr: usize, bytes: &[u8]) -> Result<(), i32> {
        let mut to = kernel_hal::user::UserOutPtr::<u8>::from(addr);
        to.write_array(bytes).map_err(|_| drm_remote::ioctl::EFAULT)
    }

    fn fence(&self, f: &Fence) -> Option<(usize, u32)> {
        self.2.as_ref().and_then(|check| check(f))
    }

    fn flags(&self) -> Option<drm_remote::Flags> {
        match self.1.load(Ordering::Relaxed) {
            NO_FLAGS => None,
            bits => Some(drm_remote::Flags(bits)),
        }
    }
}

/// The cards processes drive, in the order they were served. The kernel's
/// own device list (`kernel_hal::drivers::all_drm`) does not have them, so
/// whoever asks for every card asks here too (`drm::every_card`).
static CARDS: lock::Mutex<Vec<Arc<dyn DrmScheme>>> = lock::Mutex::new(Vec::new());

/// The cards processes drive now.
pub fn served_cards() -> Vec<Arc<dyn DrmScheme>> {
    CARDS.lock().clone()
}

/// The most cards [`served_snapshot`] holds.
pub const SNAPSHOT_MAX: usize = 8;

/// Whether a GPU a process drives reaches memory only through an IOMMU
/// domain of its own: then a frame the kernel holds is not memory it can
/// read, and it must not be offered the copy of one
/// ([`ce_presents_allowed`]).
static DMA_ISOLATED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// A GPU a process drives was handed over behind an IOMMU domain.
pub fn note_dma_isolated() {
    DMA_ISOLATED.store(true, core::sync::atomic::Ordering::Release);
}

/// Whether the cards processes drive may take the copy of a frame from the
/// kernel's memory with their copy engines. Not behind an IOMMU: the frame
/// is not in the GPU's domain, the copy engine reads nothing (the IOMMU
/// faults it) and the screen goes black while every copy is said done. The
/// CPU copies those frames until the frame is lent to the GPU's domain.
pub fn ce_presents_allowed() -> bool {
    !DMA_ISOLATED.load(core::sync::atomic::Ordering::Acquire)
}

/// [`served_snapshot`], empty when their copy engines may not take the
/// kernel's frames ([`ce_presents_allowed`]): what the present path offers
/// a copy to.
pub fn ce_snapshot() -> [Option<Arc<dyn DrmScheme>>; SNAPSHOT_MAX] {
    if ce_presents_allowed() {
        served_snapshot()
    } else {
        Default::default()
    }
}

/// The cards processes drive now, without the heap: for the present path,
/// which runs every frame and must not allocate.
pub fn served_snapshot() -> [Option<Arc<dyn DrmScheme>>; SNAPSHOT_MAX] {
    let mut snapshot: [Option<Arc<dyn DrmScheme>>; SNAPSHOT_MAX] = Default::default();
    for (slot, card) in snapshot.iter_mut().zip(CARDS.lock().iter()) {
        *slot = Some(card.clone());
    }
    snapshot
}

/// `card` is one a process drives, from now on.
pub(crate) fn note_served(card: &Arc<dyn DrmScheme>) {
    let mut cards = CARDS.lock();
    if !cards.iter().any(|c| Arc::ptr_eq(c, card)) {
        cards.push(card.clone());
    }
}

/// `card`'s process is gone.
pub(crate) fn forget_served(card: &Arc<dyn DrmScheme>) {
    CARDS.lock().retain(|c| !Arc::ptr_eq(c, card));
}

/// What [`serve_card`] made: the card, for whoever watches its server.
/// The cards of a GPU (one a nouveau client can use) a process serves now:
/// what `/proc/gpuready` waits for on a boot that leaves its NVIDIA GPUs to
/// `udrv-nvidia`.
static SERVED_GPUS: AtomicUsize = AtomicUsize::new(0);

/// `/proc/gpuready`'s lines for the `left` NVIDIA GPUs the boot leaves to
/// `udrv-nvidia` (`udriver=nvidia@N`), of which processes serve `served`
/// cards now: one not served yet is one a GPU session must wait for, as
/// for the kernel's console GPU, or the compositor starts on no GPU.
pub fn left_to_userspace(left: usize, served: usize) -> impl Iterator<Item = (bool, String)> {
    (0..left).map(move |n| {
        let up = n < served;
        let state = if up { "served" } else { "not served yet" };
        (
            up,
            alloc::format!("nvidia@{n}: left to udrv-nvidia, {state}"),
        )
    })
}

/// How many NVIDIA GPUs the boot leaves to userspace (`udriver=nvidia@N`).
pub fn left_to_udrv_nvidia() -> usize {
    kernel_hal::drivers::user::reserved()
        .iter()
        .filter(|d| d.starts_with("nvidia@"))
        .count()
}

/// How many cards of a GPU processes serve now ([`SERVED_GPUS`]).
pub fn served_gpus() -> usize {
    SERVED_GPUS.load(Ordering::SeqCst)
}

pub struct ServedCard {
    link: Arc<Link>,
    card: Arc<dyn DrmScheme>,
}

impl ServedCard {
    /// Read what the server wrote. Whether it is still there.
    pub fn look(&self) -> bool {
        self.link.pump();
        !self.link.is_dead()
    }

    /// How the server is ended when it hangs.
    pub fn killed_with(&self, kill: Kill) {
        self.link.killed_with(kill);
    }

    /// The server is gone: the card leaves the DRM core, and its node
    /// answers no more.
    pub fn gone(&self) {
        self.link.close();
        forget_served(&self.card);
        if self.card.nouveau_uapi_capable() {
            SERVED_GPUS.fetch_sub(1, Ordering::SeqCst);
        }
        drm::unregister_driver(&self.card);
        drm::build_gpu_nodes();
        info!("served drm: the card {} is gone", self.card.name());
    }
}

/// Make the card the process at the other end of `fifo` drives, and put it
/// beside the others: in the DRM core's list and as a node in `/dev/dri`.
/// `hello` is what the driver says of itself (`drm_remote::op::HELLO`'s
/// answer, sent with the call: the server cannot answer while it makes it).
/// `fence` says where the kernel reads the driver's fences ([`FenceCheck`]).
/// `EPERM` for a process that may not; `EINVAL` for a window that holds no
/// slot, a FIFO of other messages or a `hello` that does not parse.
pub fn serve_card(
    proc: &LinuxProcess,
    fifo: Arc<Fifo>,
    window: Arc<VmObject>,
    hello: &[u8],
    fence: FenceCheck,
) -> LxResult<ServedCard> {
    if !proc.capable(crate::process::CAP_SYS_ADMIN) {
        return Err(LxError::EPERM);
    }
    let koid = fifo.id();
    let link = Arc::new(Link::new(fifo, window).map_err(|_| LxError::EINVAL)?);
    let remote = Remote::with_hello(OverLink::new(link.clone(), Some(fence)), hello)
        .map_err(|_| LxError::EINVAL)?;
    super::remember(koid, &link);
    let card: Arc<dyn DrmScheme> = Arc::new(remote);
    enable_nouveau_uapi(&*card);
    note_served(&card);
    if card.nouveau_uapi_capable() {
        SERVED_GPUS.fetch_add(1, Ordering::SeqCst);
    }
    drm::register_driver(card.clone());
    drm::build_gpu_nodes();
    add_nodes();
    info!("served drm: the card {} is served", card.name());
    Ok(ServedCard { link, card })
}

/// `nvidia.nouveau_uapi` on the boot line, which no kernel card could take
/// then: a served card that has it takes it now, as the kernel's would have
/// at boot. Before the card is registered, so its node is a nouveau one
/// from the first open.
#[cfg(target_arch = "x86_64")]
fn enable_nouveau_uapi(card: &dyn DrmScheme) {
    use zcore_drivers::display::{
        nouveau_uapi_enabled, nouveau_uapi_requested, set_nouveau_uapi_enabled,
    };
    if nouveau_uapi_requested() && !nouveau_uapi_enabled() && card.nouveau_uapi_capable() {
        set_nouveau_uapi_enabled(true);
        info!(
            "served drm: nvidia.nouveau_uapi enabled for the served card {}",
            card.name()
        );
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn enable_nouveau_uapi(_card: &dyn DrmScheme) {}

/// A `/dev/dri` node for every card that has none: one served after the
/// boot made the nodes, and `/dev/dri` itself on a machine whose boot found
/// no card to make it for.
fn add_nodes() {
    use rcore_fs_devfs::DevINode;
    let Some(root) = crate::fs::DEVFS_ROOT.lock().clone() else {
        return;
    };
    let found = (root.clone() as Arc<dyn INode>).find("dri");
    let dri: Arc<dyn INode> = match found {
        Ok(dri) => dri,
        Err(_) => match root.add_dir("dri") {
            Ok(dir) => dir,
            // Made by a racing serve between the look and the add.
            Err(_) => match (root as Arc<dyn INode>).find("dri") {
                Ok(dri) => dri,
                Err(_) => return,
            },
        },
    };
    let Some(dir) = dri.downcast_ref::<DevINode>() else {
        return;
    };
    for node in drm::gpu_nodes() {
        for minor in [node.card_minor(), node.render_minor()] {
            let name = drm::node_name(minor);
            if dri.find(&name).is_err() {
                let _ = dir.add(&name, Arc::new(crate::fs::devfs::DrmDev::new(minor)));
            }
        }
    }
}

#[cfg(test)]
mod tests;
