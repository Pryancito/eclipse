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

/// How long the deferred console bring-up keeps scanout paused and console
/// presents held: the kernel's own [`SCANOUT_PAUSE_MAX`] for its GPUs, and
/// while a process serves a GPU, that card's bring-up patience plus a margin.
/// A shorter pause lifted while the process's state load was still rebinding
/// the console card's BAR1, and a present landing in that bind faults BAR1
/// for the rest of the boot.
///
/// [`SCANOUT_PAUSE_MAX`]: crate::fs::devfs::drm::SCANOUT_PAUSE_MAX
pub fn bringup_pause() -> core::time::Duration {
    pause_for(served_gpus())
}

/// [`bringup_pause`] with `served` cards of a GPU served.
fn pause_for(served: usize) -> core::time::Duration {
    let kernel = crate::fs::devfs::drm::SCANOUT_PAUSE_MAX;
    if served == 0 {
        return kernel;
    }
    kernel.max(core::time::Duration::from_millis(
        BRINGUP_PATIENCE_MS + 10_000,
    ))
}

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
        // driver halfway through that boot left the monitor black. So does
        // nvidia-modeset's load, with its first modeset. The patience is for
        // every link of the driver, and for as long as this request waits:
        // what its other threads are asked meanwhile waits for the RM this
        // request holds.
        let _long = matches!(
            op,
            drm_remote::op::CONSOLE_BRINGUP
                | drm_remote::op::CONSOLE_GR_LADDER
                | drm_remote::op::NVKMS_LOAD
        )
        .then(|| super::long_request(&self.0, BRINGUP_PATIENCE_MS));
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
static CARDS: lock::Mutex<Vec<Driven>> = lock::Mutex::new(Vec::new());

/// One of [`CARDS`]: the card, and the link its driver answers on (a card a
/// test put there has none).
#[derive(Clone)]
struct Driven {
    card: Arc<dyn DrmScheme>,
    link: Option<Arc<Link>>,
}

impl Driven {
    /// Whether its driver is still at the other end of the link.
    fn answers(&self) -> bool {
        self.link.as_ref().is_none_or(|link| !link.is_dead())
    }
}

/// The cards processes drive now.
pub fn served_cards() -> Vec<Arc<dyn DrmScheme>> {
    CARDS.lock().iter().map(|s| s.card.clone()).collect()
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

/// Of the cards processes drive, those the present path offers the copy
/// of a frame to. All of them, when their copy engines may take the
/// kernel's frames ([`ce_presents_allowed`]). Behind an IOMMU, none for a
/// frame in the kernel's memory; and for one in a card's own (`in_vram`: a
/// buffer its driver placed in VRAM), the cards with the monitor. Such a
/// card copies its own VRAM to its own framebuffer and reads nothing of
/// the kernel's, which is how the kernel's driver presents those frames;
/// a card without the monitor would write the frame into another card's
/// memory, and that is not in its domain either. Whose VRAM the frame is
/// in is the driver's to say: it declines another card's.
pub fn ce_snapshot(in_vram: bool) -> [Option<Arc<dyn DrmScheme>>; SNAPSHOT_MAX] {
    let mut snapshot = served_snapshot();
    let allowed = ce_presents_allowed();
    for slot in snapshot.iter_mut() {
        let console = slot.as_ref().is_some_and(|card| card.is_console_gpu());
        if !copy_offered(allowed, in_vram, console) {
            *slot = None;
        }
    }
    snapshot
}

/// [`ce_snapshot`] for one card: `allowed`, the cards may copy the
/// kernel's frames; `in_vram`, the frame is in a card's own memory;
/// `console`, this card has the monitor.
fn copy_offered(allowed: bool, in_vram: bool, console: bool) -> bool {
    allowed || (in_vram && console)
}

/// The cards processes drive now, without the heap: for the present path,
/// which runs every frame and must not allocate.
pub fn served_snapshot() -> [Option<Arc<dyn DrmScheme>>; SNAPSHOT_MAX] {
    let mut snapshot: [Option<Arc<dyn DrmScheme>>; SNAPSHOT_MAX] = Default::default();
    for (slot, served) in snapshot.iter_mut().zip(CARDS.lock().iter()) {
        *slot = Some(served.card.clone());
    }
    snapshot
}

/// A GPU's place on the bus (`DrmScheme::pci_bdf`).
type Place = (u32, u8, u8, u8);

/// What the boot keeps of nvidia-modeset and the console GPU a process
/// drives.
struct Nvkms {
    /// The boot gives that GPU to nvidia-modeset ([`expect_nvkms`]).
    expected: bool,
    /// The GPUs whose driver was asked to load it ([`nvkms_load_due`]).
    asked: Vec<Place>,
    /// Those whose driver answered.
    answered: Vec<Place>,
}

static NVKMS: lock::Mutex<Nvkms> = lock::Mutex::new(Nvkms {
    expected: false,
    asked: Vec::new(),
    answered: Vec::new(),
});

/// The boot is going to ask the driver of the GPU that scans out the
/// console, one a process drives, to load nvidia-modeset
/// ([`nvkms_load_due`]): `nvidia.nvkms` on a boot that brings that GPU up.
/// Until its driver has answered, that card is offered no frame to flip to
/// ([`flip_offered`]).
pub fn expect_nvkms() {
    NVKMS.lock().expected = true;
}

/// `nvidia.nvkms` for the console GPU of a process: ask the driver of each
/// served card that scans out the console, and was not asked yet, to load
/// nvidia-modeset and take the card, and say what each answered. The boot
/// gives the kernel's own cards to nvidia-modeset out of the kernel's
/// device list; these are not in it, and come when their process serves
/// them.
///
/// Only the card with the console: there this load is the modeset that
/// puts the console back on the monitor after the GPU's bring-up. The other
/// GPUs of a process stay as they are on a boot that brings no console GPU
/// up, where nvidia-modeset is not loaded in the process at all.
///
/// Once a boot for each GPU, whatever became of it: a driver that started
/// again serves the same GPUs, and a load that killed it would kill each
/// new one in turn.
pub fn nvkms_load_due() -> Vec<String> {
    let mut lines = Vec::new();
    while let Some((served, place)) = next_due() {
        // With no lock held: a call to the driver, of seconds.
        lines.extend(served.card.nvkms_load());
        // An answer is a driver's that is still there. One the load killed
        // answered nothing, and the card that comes after it at this place
        // is offered neither the pointer nor a frame.
        if served.answers() {
            NVKMS.lock().answered.push(place);
        }
    }
    lines
}

/// The next card [`nvkms_load_due`] asks, its place marked asked. One at a
/// time, each marked when its own turn comes: a card whose driver died of
/// an earlier card's load is left for the driver that starts again.
fn next_due() -> Option<(Driven, Place)> {
    // Dropped after the guard below: one of these may be a card's last
    // reference, which is not for dropping under a lock.
    let cards = CARDS.lock().clone();
    let mut nvkms = NVKMS.lock();
    let (served, place) = due(&cards, &nvkms.asked)?;
    nvkms.asked.push(place);
    drop(nvkms);
    Some((served.clone(), place))
}

/// Of `cards`, the first that scans out the console, whose driver is
/// there, at a place not in `asked`. A card with no place on the bus is no
/// GPU's.
fn due<'a>(cards: &'a [Driven], asked: &[Place]) -> Option<(&'a Driven, Place)> {
    cards.iter().find_map(|served| {
        let place = served.card.pci_bdf()?;
        let due = served.card.is_console_gpu() && served.answers() && !asked.contains(&place);
        due.then_some((served, place))
    })
}

/// How many GPUs [`nvkms_load_due`] has asked for.
pub fn nvkms_asked() -> usize {
    NVKMS.lock().asked.len()
}

/// Whether the cursor plane of `card`, one a process drives, may be
/// offered the pointer: once its driver has answered nvidia-modeset's load
/// ([`nvkms_load_due`]), whatever it answered. The plane is
/// nvidia-modeset's then, and the only pointer there is on a card it flips
/// to the compositor's own buffers.
///
/// Not before: a GPU whose RM is up and that nvidia-modeset has not taken
/// yet brings up its own cursor channel when it is handed a pointer, on
/// the display's one core channel, and nvidia-modeset then refuses the
/// card: no console modeset, and the monitor stays black. And never on a
/// boot that asks for no load, where the kernel draws the pointer of a
/// served card, as it always has.
pub fn cursor_plane_offered(card: &dyn DrmScheme) -> bool {
    card.pci_bdf()
        .is_some_and(|place| NVKMS.lock().answered.contains(&place))
}

/// Whether `card` may be offered a frame to flip to
/// (`DrmScheme::page_flip`): any card but the one nvidia-modeset is still
/// to take ([`awaited`]). Handed a frame, a GPU whose RM is up brings up a
/// window channel of its own on the display's one core channel, and
/// nvidia-modeset then refuses the card. Until its driver has answered
/// the load, the kernel copies that card's frames itself.
pub fn flip_offered(card: &Arc<dyn DrmScheme>) -> bool {
    let served = CARDS.lock().iter().any(|s| Arc::ptr_eq(&s.card, card));
    if !served {
        return true;
    }
    let nvkms = NVKMS.lock();
    !awaited(
        nvkms.expected,
        card.is_console_gpu(),
        card.pci_bdf(),
        &nvkms.answered,
    )
}

/// Whether nvidia-modeset is still to take a served card: on a boot that
/// gives the console GPU of a process to it (`expected`), the card that
/// scans out the console (`console`), until the driver of the GPU at its
/// `place` has answered the load.
fn awaited(expected: bool, console: bool, place: Option<Place>, answered: &[Place]) -> bool {
    expected && console && !place.is_some_and(|place| answered.contains(&place))
}

/// `card` is one a process drives, from now on: a test's, with no driver
/// at the other end of anything.
#[cfg(test)]
pub(crate) fn note_served(card: &Arc<dyn DrmScheme>) {
    note_served_over(card, None);
}

/// `card` is one a process drives, from now on, its driver answering on
/// `link`.
fn note_served_over(card: &Arc<dyn DrmScheme>, link: Option<Arc<Link>>) {
    let mut cards = CARDS.lock();
    if !cards.iter().any(|s| Arc::ptr_eq(&s.card, card)) {
        cards.push(Driven {
            card: card.clone(),
            link,
        });
    }
}

/// `card`'s process is gone.
pub(crate) fn forget_served(card: &Arc<dyn DrmScheme>) {
    CARDS.lock().retain(|s| !Arc::ptr_eq(&s.card, card));
}

/// What [`serve_card`] made: the card, for whoever watches its server.
/// The cards of a GPU (one a nouveau client can use) a process serves now:
/// what `/proc/gpuready` waits for on a boot that leaves its NVIDIA GPUs to
/// `udrv-nvidia`.
static SERVED_GPUS: AtomicUsize = AtomicUsize::new(0);

/// How long the GPUs the boot leaves to `udrv-nvidia` are waited for to be
/// served, from the first time anything asked whether they were
/// ([`serve_overdue`]). That driver serves no card before its GPUs have
/// their RM (8 s for one RTX 2060); one that never serves must hold
/// neither a session nor the console GPU's bring-up for good.
const SERVE_PATIENCE_MS: u64 = 60_000;

/// When [`serve_overdue`] was first asked, in ms since boot. 0: not yet.
static SERVE_ASKED_MS: AtomicU64 = AtomicU64::new(0);

/// Whether the GPUs left to `udrv-nvidia` have had their time to be
/// served ([`SERVE_PATIENCE_MS`]): whoever waits for one stops then. The
/// first to ask starts the clock: init, asking `/proc/gpuready` whether
/// its session can start, or the console GPU's deferred bring-up.
pub fn serve_overdue() -> bool {
    let now = (kernel_hal::timer::timer_now().as_millis() as u64).max(1);
    let since = match SERVE_ASKED_MS.compare_exchange(0, now, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => now,
        Err(since) => since,
    };
    overdue(since, now)
}

/// [`serve_overdue`] at `now_ms`, first asked at `since_ms`.
fn overdue(since_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(since_ms) >= SERVE_PATIENCE_MS
}

/// `/proc/gpuready`'s lines for the `left` NVIDIA GPUs the boot leaves to
/// `udrv-nvidia` (`udriver=nvidia@N`), of which processes serve `served`
/// cards now: one not served yet is one a GPU session must wait for, as
/// for the kernel's console GPU, or the compositor starts on no GPU. Not
/// once they are `overdue` ([`serve_overdue`]): nothing is coming.
pub fn left_to_userspace(
    left: usize,
    served: usize,
    overdue: bool,
) -> impl Iterator<Item = (bool, String)> {
    (0..left).map(move |n| {
        let up = n < served;
        let state = match (up, overdue) {
            (true, _) => "served",
            (false, false) => "not served yet",
            (false, true) => "never served",
        };
        (
            up || overdue,
            alloc::format!("nvidia@{n}: left to udrv-nvidia, {state}"),
        )
    })
}

/// The console-GSP gate was opened by the boot's deferred console
/// bring-up, for its window ([`console_gate_opened_for_its_window`]).
static GATE_IS_THE_WINDOWS: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// The deferred console bring-up is opening the console-GSP gate, with the
/// scanout parked. A driver's process is told with the bring-up's own
/// request (`drm_remote::op::CONSOLE_BRINGUP`), so from here on the
/// switches it reads when it starts say the gate is shut
/// ([`console_gate_is_the_windows`]): a driver that started again after
/// the window found it open, and brought the console GPU up for its first
/// client with the scanout live and nothing held, which is what the window
/// is there to prevent.
pub fn console_gate_opened_for_its_window() {
    GATE_IS_THE_WINDOWS.store(true, Ordering::Release);
}

/// Whether the console-GSP gate, if open, is open only for the deferred
/// bring-up's window.
pub fn console_gate_is_the_windows() -> bool {
    GATE_IS_THE_WINDOWS.load(Ordering::Acquire)
}

/// `/proc/gpuready`'s lines for the `left` NVIDIA GPUs the boot leaves to
/// `udrv-nvidia`: whether each is served ([`left_to_userspace`]), and
/// whether a client can use the one that scans out the console
/// ([`console_gpus_ready`]).
pub fn gpuready_lines(left: usize) -> Vec<(bool, String)> {
    left_to_userspace(left, served_gpus(), serve_overdue())
        .chain(console_gpus_ready())
        .collect()
}

/// How `/proc/gpuready` marks the line of a console GPU a process drives.
pub const IN_A_PROCESS: &str = "in udrv-nvidia";

/// `/proc/gpuready`'s lines for the console GPUs processes drive.
///
/// The session waits for one the boot is still to bring up, as it waits
/// for the kernel's own. Started before, the compositor finds a GPU with no
/// RM and falls back to the CPU for the whole session, and its start (a VT
/// switch, a modeset) lands in the window the bring-up needs silent: the
/// driver's process said nothing of where its console GPU was, so the boot
/// read "served" as "ready" and started the session at once.
pub fn console_gpus_ready() -> Vec<(bool, String)> {
    let coming = drm::console_bringup_unfinished();
    let (expected, answered) = {
        let nvkms = NVKMS.lock();
        (nvkms.expected, nvkms.answered.clone())
    };
    served_cards()
        .iter()
        .filter_map(|card| {
            let (ready, line) = card.console_gpu_ready()?;
            let modeset = !awaited(expected, card.is_console_gpu(), card.pci_bdf(), &answered);
            waited_for(coming, ready, modeset, &line)
        })
        .collect()
}

/// One console GPU's line of [`console_gpus_ready`], when it has one, from
/// the driver's own `line`: `coming`, the boot's bring-up of it is still
/// ahead; `ready`, a client can use it; `modeset`, nvidia-modeset has been
/// given it, or is not going to be.
///
/// One a bring-up left cold holds nobody, like one no bring-up was
/// scheduled for: nothing more is coming. One that came up is waited for
/// until nvidia-modeset has it: a compositor's first pointer on a GPU with
/// its RM up takes the display's core channel, and nvidia-modeset is then
/// refused the card ([`cursor_plane_offered`]).
fn waited_for(coming: bool, ready: bool, modeset: bool, line: &str) -> Option<(bool, String)> {
    let line = alloc::format!("{line}, {IN_A_PROCESS}");
    match (ready, modeset) {
        (true, true) => Some((true, line)),
        (true, false) => Some((false, line + ", nvidia-modeset not loaded yet")),
        (false, _) if coming => Some((false, line)),
        (false, _) => None,
    }
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

    /// The process that drives the card (`server`, its id), and how it is
    /// ended when it hangs.
    pub fn killed_with(&self, server: u64, kill: Kill) {
        self.link.killed_with(server, kill);
    }

    /// The server is gone: the card leaves the DRM core, and its node
    /// answers no more.
    pub fn gone(&self) {
        self.link.close();
        forget_served(&self.card);
        if self.card.nouveau_uapi_capable() {
            SERVED_GPUS.fetch_sub(1, Ordering::SeqCst);
        }
        // The pointer this card's cursor plane had is the kernel's to draw
        // again: nothing else says so before the compositor next sets an
        // image.
        let had_the_monitor =
            drm::get_display_driver().is_some_and(|panel| Arc::ptr_eq(&panel, &self.card));
        drm::unregister_driver(&self.card);
        if had_the_monitor {
            drm::cursor_blit::pointer_plane_lost();
        }
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
    note_served_over(&card, Some(link.clone()));
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
