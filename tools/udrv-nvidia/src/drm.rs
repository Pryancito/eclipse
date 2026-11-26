//! The card this process drives, served to the kernel's DRM core
//! (`ECLIPSE_DRM_SERVE`): every call the core makes of it is a request on
//! the node's FIFO, answered by `drm_remote::answer` on the driver, which
//! is the kernel's own driver and does not know the difference.
use std::sync::{Arc, Mutex};

use udrivers::served::{Msg, Node, SLOT_LEN};
use zcore_drivers::scheme::drm_remote::{self, EINVAL};
use zcore_drivers::scheme::DrmScheme;

/// Serve `card` until the kernel lets go of it. Only returns then, with
/// why; `Err` when it could not be served at all.
pub fn serve(card: Arc<dyn DrmScheme>) -> Result<String, String> {
    let node = Node::serve_drm(&drm_remote::hello(&*card))?;
    let why = node.run_with(&Mutex::new(()), |_, node, req| {
        let answer = reply(&*card, node, req);
        if req.op == drm_remote::op::NVKMS_LOAD {
            nvkms_timers(&card);
        }
        // A file closing, a process gone: told, and answered by nothing.
        (req.reqid != NO_REPLY).then_some(answer)
    });
    Ok(format!("{why}"))
}

/// The `reqid` of a message the kernel waits on no answer for
/// (`served::link::NO_REPLY`).
const NO_REPLY: u32 = u32::MAX;

/// The cards nvidia-modeset was asked to take (`drm_remote::op::NVKMS_LOAD`).
static NVKMS_CARDS: Mutex<Vec<Arc<dyn DrmScheme>>> = Mutex::new(Vec::new());

/// Between two looks at nvidia-modeset's timers: no less, so a timer due at
/// once does not spin a thread; no more, for a timer a later call queues
/// while the thread sleeps, since nothing wakes it.
const TIMERS_EVERY: std::ops::RangeInclusive<u64> = 1_000..=100_000;

/// Run nvidia-modeset's timers from now on, on a thread of their own: Linux
/// runs them from nvidia-modeset's kthread, and the kernel's driver from
/// the task that loaded it. They are one queue for every card, so one
/// thread, started by the first card asked; a card nvidia-modeset did not
/// take runs none (`nvkms_service` is `None`), and the next is asked.
fn nvkms_timers(card: &Arc<dyn DrmScheme>) {
    let mut cards = NVKMS_CARDS.lock().unwrap();
    let first = cards.is_empty();
    if !cards.iter().any(|c| Arc::ptr_eq(c, card)) {
        cards.push(card.clone());
    }
    drop(cards);
    if !first {
        return;
    }
    std::thread::spawn(|| loop {
        let cards = NVKMS_CARDS.lock().unwrap().clone();
        let next = cards.iter().filter_map(|card| card.nvkms_service()).min();
        let wait = next
            .unwrap_or(*TIMERS_EVERY.end())
            .clamp(*TIMERS_EVERY.start(), *TIMERS_EVERY.end());
        std::thread::sleep(std::time::Duration::from_micros(wait));
    });
}

/// The requests answered so far, for `drm-check` to say how many calls an
/// ioctl costs.
pub static ANSWERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The answer to one request, its bytes put in its slot.
fn reply(card: &dyn DrmScheme, node: &Node, req: &Msg) -> Msg {
    ANSWERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let slot = node.slot_of(req);
    let carried = (req.len as usize).min(SLOT_LEN);
    let mut out = Vec::new();
    let result = drm_remote::answer(card, req.op, req.a, req.b, &slot[..carried], &mut out);
    let mut reply = Msg {
        op: req.op,
        reqid: req.reqid,
        // The card's flags, with every answer: the kernel reads them
        // under its DRM lock, where it must not call.
        fh: drm_remote::Flags::of(card).0 as u32,
        ..Msg::default()
    };
    match result {
        Ok(word) if out.len() <= SLOT_LEN => {
            slot[..out.len()].copy_from_slice(&out);
            reply.a = word;
            reply.len = out.len() as u32;
        }
        Ok(_) => reply.b = (-(EINVAL as i64)) as u64,
        Err(errno) => reply.b = (-(errno as i64)) as u64,
    }
    reply
}

/// A card with nothing behind it, for `udrv-nvidia drm-check`: one CRTC
/// and one connector, by numbers no other card uses, and framebuffers
/// and flips it only counts.
pub mod check {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use zcore_drivers::scheme::drm::{DrmCaps, DrmConnector, DrmCrtc, DrmPlane};
    use zcore_drivers::scheme::{DrmScheme, Scheme};

    pub const CRTC: u32 = 0x5a_0031;

    /// `DRM_IOCTL_NOUVEAU_EXEC`: `_IOWR('d', 0x52, struct drm_nouveau_exec)`.
    pub const EXEC: u32 = 0xc028_6452;

    /// `struct drm_nouveau_exec`.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug, PartialEq)]
    pub struct Exec {
        pub channel: u32,
        pub push_count: u32,
        pub wait_count: u32,
        pub sig_count: u32,
        pub wait_ptr: u64,
        pub sig_ptr: u64,
        pub push_ptr: u64,
    }
    pub const CONNECTOR: u32 = 0x5a_0041;

    #[derive(Default)]
    pub struct Card {
        pub flips: AtomicU32,
        /// Buffers the kernel made and told it of, and then freed.
        pub imported: AtomicU32,
        pub freed: AtomicU32,
        /// The kernel asked which of an EXEC's waits are the CPU's.
        pub waits_asked: AtomicU32,
        /// Where it says it is on the bus: nowhere, but for `nvkms-check`,
        /// which the kernel asks what it asks the card of a GPU.
        pub place: Option<(u32, u8, u8, u8)>,
        /// It says it scans out the console: the card nvidia-modeset is
        /// asked for.
        pub console: bool,
        /// The times nvidia-modeset was asked for, and the phases of the
        /// boot its heads were asked at.
        pub nvkms_loads: AtomicU32,
        pub liveness: std::sync::Mutex<Vec<String>>,
        /// The kernel asked it to bring the console GPU up, and then for
        /// its GR ladder: a client can use it, as far as it says.
        pub brought_up: AtomicBool,
        pub ladder: AtomicBool,
    }

    impl Scheme for Card {
        fn name(&self) -> &str {
            "udrv-nvidia-check"
        }
    }

    impl DrmScheme for Card {
        fn get_caps(&self) -> DrmCaps {
            DrmCaps {
                has_3d: false,
                has_cursor: false,
                max_width: 1024,
                max_height: 768,
            }
        }
        // Its own monitors, so its node lists them, and no hardware KMS,
        // so the panel stays where it is.
        fn drives_own_display(&self) -> bool {
            true
        }
        fn pci_bdf(&self) -> Option<(u32, u8, u8, u8)> {
            self.place
        }
        fn is_console_gpu(&self) -> bool {
            self.console
        }
        /// As the NVIDIA driver answers: nothing for a GPU that does not
        /// scan out the console, and ready once the boot brought it up.
        fn console_gpu_ready(&self) -> Option<(bool, String)> {
            let up = self.brought_up.load(Ordering::Relaxed) && self.ladder.load(Ordering::Relaxed);
            self.console.then(|| (up, String::new()))
        }
        fn deferred_console_bringup(&self) -> String {
            if !self.console {
                return String::new();
            }
            self.brought_up.store(true, Ordering::Relaxed);
            "udrv-nvidia-check: asked to bring the console GPU up".into()
        }
        fn build_console_gr_ladder(&self) -> String {
            if !self.console {
                return String::new();
            }
            self.ladder.store(true, Ordering::Relaxed);
            "udrv-nvidia-check: asked for the console GPU's GR ladder".into()
        }
        fn nvkms_load(&self) -> Vec<String> {
            let time = self.nvkms_loads.fetch_add(1, Ordering::Relaxed) + 1;
            vec![format!(
                "udrv-nvidia-check: asked for nvidia-modeset, time {time}"
            )]
        }
        fn scanout_liveness(&self, phase: &str) -> String {
            self.liveness.lock().unwrap().push(phase.into());
            format!("[udrv-nvidia-check] {phase}: a card with no head\n")
        }
        fn create_fb(&self, handle: u32, _w: u32, _h: u32, _pitch: u32) -> Option<u32> {
            Some(0x5a_0000 + handle)
        }
        fn import_buffer(&self, h: zcore_drivers::scheme::drm::GemHandle) -> bool {
            self.imported.fetch_add(1, Ordering::Relaxed);
            h.phys_addr != 0
        }
        fn free_buffer(&self, _h: zcore_drivers::scheme::drm::GemHandle) {
            self.freed.fetch_add(1, Ordering::Relaxed);
        }
        /// Every wait of an `EXEC` is the CPU's, read from its array as
        /// the real driver reads it.
        fn exec_cpu_waits(&self, arg: usize, _pid: u64) -> Option<(Vec<u32>, Vec<u64>)> {
            self.waits_asked.fetch_add(1, Ordering::Relaxed);
            // SAFETY: what the kernel's end copied: the struct, and
            // `wait_count` waits where `wait_ptr` says.
            let e = unsafe { &*(arg as *const Exec) };
            if e.wait_count == 0 {
                return None;
            }
            let waits = unsafe {
                core::slice::from_raw_parts(e.wait_ptr as *const [u32; 4], e.wait_count as usize)
            };
            Some((waits.iter().map(|w| w[1]).collect(), vec![1; waits.len()]))
        }
        fn page_flip(&self, _fb: u32) -> bool {
            self.flips.fetch_add(1, Ordering::Relaxed);
            true
        }
        fn set_cursor(&self, _: u32, _: i32, _: i32, _: u32, _: u32) -> bool {
            false
        }
        fn wait_vblank(&self, crtc: u32) -> bool {
            crtc == CRTC
        }
        /// `EXEC` ([`Exec`]): the sum of its pushes' `va`, in `channel`,
        /// read from the arrays as the real driver reads them, through the
        /// pointers in its struct. Every other ioctl is ENOSYS.
        fn ioctl_file(&self, cmd: u32, arg: usize, _pid: u64, _file: usize) -> Result<usize, i32> {
            if cmd != EXEC {
                return Err(38);
            }
            // SAFETY: what the kernel's end copied: `_IOC_SIZE` bytes of
            // struct, and `push_count` pushes where `push_ptr` says.
            let e = unsafe { &mut *(arg as *mut Exec) };
            let pushes = match e.push_count {
                0 => &[][..],
                n => unsafe {
                    core::slice::from_raw_parts(e.push_ptr as *const [u64; 2], n as usize)
                },
            };
            e.channel = pushes.iter().map(|p| p[0] as u32).sum();
            Ok(0)
        }
        fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
            (vec![], vec![CRTC], vec![CONNECTOR])
        }
        fn get_connector(&self, id: u32) -> Option<DrmConnector> {
            (id == CONNECTOR).then_some(DrmConnector {
                id,
                connected: true,
                mm_width: 300,
                mm_height: 200,
                connector_type: 14,
            })
        }
        fn get_crtc(&self, id: u32) -> Option<DrmCrtc> {
            (id == CRTC).then_some(DrmCrtc {
                id,
                fb_id: 0,
                x: 0,
                y: 0,
            })
        }
        fn get_plane(&self, _: u32) -> Option<DrmPlane> {
            None
        }
        fn get_planes(&self) -> Vec<u32> {
            vec![]
        }
        fn set_plane(
            &self,
            _: u32,
            _: u32,
            _: u32,
            _: i32,
            _: i32,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
        ) -> bool {
            false
        }
    }
}
