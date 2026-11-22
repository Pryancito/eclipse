//! `nvidia.nvkms`: give this GPU's display to nvidia-modeset, the way
//! `nvidia-drm.modeset=1` does in Linux. `docs/README-nvkms-port.md`.
//!
//! Every card gets an nvkms device -- nvkms's own core channel and display
//! state for it -- and the boot log gets what nvkms sees on it (PR 3). The
//! card scanning out the firmware console then gets the first modeset (PR 4):
//! nvkms takes the console memory as its primary surface and sets each
//! monitor to a mode of the console's size, so the screen shows the same
//! pixels, now scanned out by nvkms instead of by what the firmware left.
//!
//! From then on the compositor's VRAM framebuffers are flipped to through
//! kapi, and the pointer is nvkms's cursor plane, as with nvidia-drm. A frame
//! nvkms cannot flip to (a sysmem or PRIME buffer) is copied into the console
//! by the CPU or the copy engine as before, with the console put back on
//! screen first.
//!
//! nvkms and this driver's own display ladder (the NVC57E surface flip and
//! the NVC57A cursor) cannot share a card, because both allocate its core
//! channel. Whichever comes first keeps it: nvkms claims both ladder states
//! with [`NVKMS_OWNS`], and refuses a card whose ladder is already up.
//!
//! Own file for the size ceiling of `nvidia.rs`.

use super::{NvidiaGpu, NvidiaKmsFramebuffer};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use nvidia_rm_sys::nvkms::{
    self, KmsDevice, KmsHeadSet, KmsInfo, KmsMode, KmsScanout, KmsSurface, ShowError,
};
use nvidia_rm_sys::nvkms_os;

/// What nvkms holds of one card, for the rest of the boot: nvkms has its core
/// channel, so nothing hands the card back.
pub(super) struct NvkmsCard {
    device: KmsDevice,
    /// What nvkms scans out: on the card that had the firmware console from
    /// the console modeset on, on any other card from a client's first
    /// modeset on.
    scanout: Option<Scanout>,
    /// The card's displays as the last probe found them: at load, and again
    /// each time the output poll saw a monitor come or go.
    info: Option<alloc::boxed::Box<KmsInfo>>,
    /// [`nvkms::hotplug_count`] when this card was last probed.
    hotplugs: u32,
    /// The output poll's own state for this card.
    poll: Poll,
}

/// What [`NvidiaGpu::nvkms_poll_connectors`] remembers between passes.
#[derive(Default)]
struct Poll {
    /// Passes so far.
    passes: u32,
    /// The connected mask the RM's last detection pass gave.
    rm_connected: Option<u32>,
    /// The connectors have been announced since nvkms took the card.
    announced: bool,
}

/// The RM detection pass runs every this many output-poll passes: 10 s at
/// the DRM layer's 2 s, Linux's `DRM_OUTPUT_POLL_PERIOD`. nvkms's own
/// hotplug events are looked at on every pass.
const RM_DETECT_EVERY: u32 = 5;

/// What the probe says about the monitors: which display is connected, and
/// which monitor it is (the EDID's vendor, product and serial).
fn monitors(info: Option<&KmsInfo>) -> Vec<(u32, [u8; 10])> {
    let Some(info) = info else {
        return Vec::new();
    };
    info.displays()
        .iter()
        .filter(|d| d.connected != 0)
        .map(|d| {
            let mut id = [0u8; 10];
            id.copy_from_slice(&d.edid[8..18]);
            (d.handle, id)
        })
        .collect()
}

/// One card's screen since the console modeset: nvidia-drm's CRTC, plane and
/// framebuffer state, reduced to one picture cloned on every lit head.
struct Scanout {
    state: KmsScanout,
    /// The firmware console's surface, on the card that had it; `None` on a
    /// card whose first modeset was a client's, which has no console to
    /// put back.
    console: Option<KmsSurface>,
    /// The surfaces made for the compositor's framebuffers, by fb id. `None`
    /// for one nvkms refused, so it is not asked again.
    fbs: Vec<(u32, Option<KmsSurface>)>,
    /// The fb on screen; 0 for the console.
    on_screen: u32,
    cursor: Option<Cursor>,
    /// The lit heads, with the display and mode index each one shows.
    sets: Vec<KmsHeadSet>,
    /// The mode a DRM client set (width, height, refresh in Hz), `None`
    /// while the heads still show the console's mode. Away from the
    /// console's mode the console no longer fits the screen, so it is never
    /// put back on it.
    mode: Option<(u32, u32, u32)>,
}

/// The cursor plane's surface, and where the pointer is.
struct Cursor {
    surface: KmsSurface,
    h_client: u32,
    h_memory: u32,
    x: i32,
    y: i32,
    visible: bool,
}

/// How long a flip waits for the previous one to land: three frames at
/// 60 Hz. A flip past it loses its frame, as `surfaceflip_drain` does.
const FLIP_WAIT_US: u64 = 50_000;

/// [`NvidiaGpu::nvkms_modes`] in the shape the DRM layer lists.
pub(super) fn display_modes(gpu: &NvidiaGpu) -> Vec<crate::scheme::drm::DisplayMode> {
    gpu.nvkms_modes()
        .iter()
        .map(|m| crate::scheme::drm::DisplayMode {
            width: m.width,
            height: m.height,
            refresh_hz: nvkms::refresh_hz(m),
            pixel_clock_khz: m.pixel_clock_khz,
            preferred: m.preferred != 0,
        })
        .collect()
}

/// [`NvidiaGpu::hw_frame_count`]: the frame counter of the head nvkms
/// scans the panel out of, read straight from `NV_PDISP_RG_DPCA(head)`
/// (`FRM_CNT`, bits 31:16; `disp/v03_00/dev_disp.h`, which the RM's own
/// `kdispReadRgLineCountAndFrameCount_v03_00` reads). Only once nvkms has
/// lit the head, which is after the GSP boot: the display PRI cluster is
/// never touched near the STARTCPU window. Lock-free, for timer interrupts.
pub(super) fn hw_frame_count(gpu: &NvidiaGpu) -> Option<u32> {
    let head = gpu.vblank_head.load(Ordering::Acquire);
    if head >= 8 || gpu._bar0 == 0 {
        return None;
    }
    let reg = gpu._bar0 + 0x0061_6330 + head as usize * 2048;
    let v = unsafe { core::ptr::read_volatile(reg as *const u32) };
    // 0xBADFxxxx: a PRI error, not a counter.
    (v >> 16 != 0xBADF).then_some(v >> 16)
}

/// [`NvidiaGpu::scans_out_block_linear`]: nvkms lit a head of this card,
/// and the compositor's buffers can be in VRAM, the only memory nvkms scans.
/// Then nvkms flips to a block-linear surface as it is, as nvidia-drm does
/// for the modifiers it lists. Lock-free: the DRM core asks it per ioctl.
pub(super) fn scans_out_block_linear(gpu: &NvidiaGpu) -> bool {
    gpu.vblank_head.load(Ordering::Acquire) < 8 && super::gem_vram::enabled()
}

/// `nvidia.nvkms` is on the cmdline: nvkms is going to take every NVIDIA
/// card's display, the way `nvidia-drm.modeset=1` does in Linux.
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Called from `zCore`'s cmdline parsing when `nvidia.nvkms` is present,
/// before init runs, so a compositor started before nvkms has loaded
/// already sees each card's CRTC.
pub fn set_requested(v: bool) {
    REQUESTED.store(v, Ordering::Relaxed);
}

/// [`NvidiaGpu::drives_own_display`]: this card lights its own monitors
/// through nvkms, as its own DRM device does in Linux: `nvidia.nvkms` (and
/// `nvidia.gem_vram`, since nvkms only scans out VRAM) on a card that is
/// not the boot console's. The console's card keeps the panel path, which
/// already reaches nvkms. True from boot on, before nvkms has loaded: DRM
/// lists the card's CRTC from the start (a compositor never looks for new
/// CRTCs), and its connectors read disconnected until nvkms holds the card
/// ([`NvidiaGpu::nvkms_display_pending`]); the output poll then sends the
/// hotplug that makes the compositor light them.
pub(super) fn drives_own_display(gpu: &NvidiaGpu) -> bool {
    REQUESTED.load(Ordering::Relaxed) && super::gem_vram::enabled() && !gpu.drives_boot_display()
}

fn now_us() -> u64 {
    unsafe { crate::bus::drivers_timer_now_as_micros() }
}

impl Scanout {
    fn surface_of(&self, fb_id: u32) -> Option<&KmsSurface> {
        if fb_id == 0 {
            return self.console.as_ref();
        }
        self.fbs
            .iter()
            .find(|(id, _)| *id == fb_id)
            .and_then(|(_, s)| s.as_ref())
    }

    /// Put fb `fb_id` (0: the console) on every lit head at the next vblank,
    /// waiting up to [`FLIP_WAIT_US`] for the previous flip to land first.
    fn flip_to(&mut self, device: &KmsDevice, fb_id: u32) -> Result<(), ShowError> {
        if self.on_screen == fb_id {
            return Ok(());
        }
        let surface = self.surface_of(fb_id).ok_or(ShowError::BadRequest)?;
        let start = now_us();
        let result = loop {
            match device.flip(&self.state, surface) {
                Err(ShowError::FlipPending) if now_us().wrapping_sub(start) < FLIP_WAIT_US => {
                    core::hint::spin_loop()
                }
                r => break r,
            }
        };
        if result.is_ok() {
            self.on_screen = fb_id;
        }
        result
    }

    /// Wait, up to [`FLIP_WAIT_US`], for the last flip to land: until then
    /// the surface it flipped away from is still being read.
    fn wait_landed(&self, device: &KmsDevice) -> bool {
        let start = now_us();
        while device.flip_pending(&self.state) == Some(true) {
            if now_us().wrapping_sub(start) >= FLIP_WAIT_US {
                return false;
            }
            core::hint::spin_loop();
        }
        true
    }
}

/// The value of `surfaceflip_state` and `hwcur_state` while nvkms owns the
/// card. Not 0, so the ladder never initialises; not 1, so it never runs.
pub(super) const NVKMS_OWNS: u8 = 3;

/// Claim one ladder state for nvkms: `Ok` if it was untried (or failed, which
/// left no channel behind), `Err` with the state if the ladder is up.
fn claim(state: &AtomicU8) -> Result<u8, u8> {
    loop {
        let cur = state.load(Ordering::Acquire);
        if cur == 1 {
            return Err(cur);
        }
        if state
            .compare_exchange(cur, NVKMS_OWNS, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(cur);
        }
    }
}

/// The bits per pixel of a firmware framebuffer, from its geometry, for
/// `screen_info.lfb_depth`: the boot handover gives a pitch and a width, and a
/// UEFI GOP framebuffer is 32-bit in practice.
pub(super) fn console_depth(width: u32, pitch: u32) -> u32 {
    if width == 0 {
        return 0;
    }
    match pitch.saturating_mul(8) / width {
        32.. => 32,
        24..=31 => 24,
        16..=23 => 16,
        bpp => bpp,
    }
}

static FIRST_FLIP: AtomicBool = AtomicBool::new(false);
static FLIP_FAILED: AtomicBool = AtomicBool::new(false);
static CONSOLE_BACK_FAILED: AtomicBool = AtomicBool::new(false);
static OFF_MODE_COPY: AtomicBool = AtomicBool::new(false);
static FIRST_BLOCK_LINEAR: AtomicBool = AtomicBool::new(false);

fn layout_name(block_linear: Option<u8>) -> String {
    match block_linear {
        None => String::from("lineal"),
        Some(h) => format!("block-linear de {} GOBs", 1u32 << h),
    }
}
static CURSOR_FAILED: AtomicBool = AtomicBool::new(false);

/// One `[nvkms]` line per boot for a per-frame event.
fn log_once(latch: &AtomicBool, line: impl FnOnce() -> String) {
    if !latch.swap(true, Ordering::Relaxed) {
        crate::klog_info!("[nvkms] {}", line());
    }
}

impl NvidiaGpu {
    fn bdf_tag(&self) -> String {
        format!(
            "{:02x}:{:02x}.0 {}",
            self.pci_bus, self.pci_device, self.gpu_model
        )
    }

    /// Whether nvkms holds this card's display. Never waits: the card is only
    /// ever busy (a flip, a cursor update) once nvkms holds it.
    pub(super) fn nvkms_owns_display(&self) -> bool {
        self.nvkms_device
            .try_lock()
            .is_none_or(|card| card.is_some())
    }

    /// Load nvkms (once per boot) and give it this GPU. One line per fact,
    /// for the boot log.
    pub(super) fn nvkms_load(&self) -> Vec<String> {
        let tag = self.bdf_tag();
        if self.nvkms_owns_display() {
            return Vec::new();
        }
        if self.rm_device_instance.lock().is_none() {
            return alloc::vec![format!("{}: sin RM, nvkms no puede tomarla", tag)];
        }

        let sf = claim(&self.surfaceflip_state);
        let hc = claim(&self.hwcur_state);
        if sf.is_err() || hc.is_err() {
            // Give back what was claimed: the ladder keeps the card.
            if let Ok(prev) = sf {
                self.surfaceflip_state.store(prev, Ordering::Release);
            }
            if let Ok(prev) = hc {
                self.hwcur_state.store(prev, Ordering::Release);
            }
            return alloc::vec![format!(
                "{}: la escalera propia del display ya tiene el canal core \
                 (surfaceflip/hwcursor); nvkms no se carga en esta GPU",
                tag
            )];
        }
        let release = |gpu: &Self| {
            gpu.surfaceflip_state.store(0, Ordering::Release);
            gpu.hwcur_state.store(0, Ordering::Release);
        };

        if !nvkms_os::module_load() {
            release(self);
            return alloc::vec![format!("{}: nvKmsModuleLoad fallo", tag)];
        }
        let Some(gpu_id) = nvkms_os::gpu_id_at(self.pci_domain, self.pci_bus, self.pci_device)
        else {
            release(self);
            return alloc::vec![format!("{}: el RM no lista esta GPU", tag)];
        };
        // Before allocateDevice, which is where nvkms imports the console.
        let console_note = if self.drives_boot_display() {
            nvkms_os::describe_console(gpu_id).err().map(|status| {
                format!(
                    "{}: el RM no acepto la consola de arranque como su region de consola \
                     ({:#x})",
                    tag, status
                )
            })
        } else {
            None
        };
        let Some(device) = KmsDevice::alloc(gpu_id) else {
            release(self);
            return alloc::vec![format!(
                "{}: allocateDevice fallo para gpu id {:#x} (el motivo, en las lineas NVKMS de arriba)",
                tag, gpu_id
            )];
        };

        let mut lines = alloc::vec![format!(
            "{}: nvkms tiene el display (gpu id {:#x})",
            tag, gpu_id
        )];
        lines.extend(console_note);
        let hotplugs = nvkms::hotplug_count();
        let info = device.probe();
        match &info {
            Some(info) => {
                for line in nvkms::describe(info) {
                    lines.push(format!("{}: {}", tag, line));
                }
            }
            None => lines.push(format!("{}: no se pudo consultar el dispositivo", tag)),
        }
        let scanout = match &info {
            Some(info) if self.drives_boot_display() => {
                self.show_console(&device, gpu_id, info, &tag, &mut lines)
            }
            // The probe failure is already in `lines`.
            None if self.drives_boot_display() => None,
            _ => {
                lines.push(format!(
                    "{}: sin consola de arranque; la pantalla no cambia hasta que un \
                     compositor haga el modeset{}",
                    tag,
                    if drives_own_display(self) {
                        " (sus monitores aparecen ahora como conectados: hotplug)"
                    } else {
                        ""
                    }
                ));
                None
            }
        };
        *self.nvkms_device.lock() = Some(NvkmsCard {
            device,
            scanout,
            info,
            hotplugs,
            poll: Poll::default(),
        });
        lines
    }

    /// The console modeset: nvkms scans out the firmware console, at a mode
    /// of its size on every monitor that has one. `None` leaves the screen as
    /// the firmware set it, and says why in `lines`.
    fn show_console(
        &self,
        device: &KmsDevice,
        gpu_id: u32,
        info: &KmsInfo,
        tag: &str,
        lines: &mut Vec<String>,
    ) -> Option<Scanout> {
        let Some(fb) = *super::BOOT_FB_INFO.lock() else {
            lines.push(format!("{}: sin geometria de la consola de arranque", tag));
            return None;
        };
        if info.vt_fb_size == 0 {
            lines.push(format!(
                "{}: nvkms no tiene importada la consola de arranque; la pantalla se \
                 queda como la dejo el firmware",
                tag
            ));
            return None;
        }
        let (sets, dark) = nvkms::console_plan(info, |d| device.modes(d), fb.width, fb.height);
        for line in dark {
            lines.push(format!("{}: {}", tag, line));
        }
        if sets.is_empty() {
            lines.push(format!(
                "{}: ningun monitor puede mostrar la consola de {}x{}; sin modeset",
                tag, fb.width, fb.height
            ));
            return None;
        }
        if !self.default_lut_reaches_vram(device, gpu_id, tag, lines) {
            return None;
        }
        let depth = console_depth(fb.width, fb.pitch);
        let Some(console) = device.console_surface(fb.width, fb.height, fb.pitch, depth) else {
            lines.push(format!(
                "{}: nvkms no acepto la consola {}x{} pitch {} ({} bpp) como superficie",
                tag, fb.width, fb.height, fb.pitch, depth
            ));
            return None;
        };
        match device.show_console(&console, &sets, info.num_heads) {
            Ok(state) => {
                for set in &sets {
                    lines.push(format!(
                        "{}: modeset hecho: pantalla {:#x} en la cabeza {} a {}x{}, escaneando la consola",
                        tag, set.display, set.head, fb.width, fb.height
                    ));
                }
                if let Some(set) = sets.first() {
                    self.vblank_head.store(set.head, Ordering::Release);
                }
                Some(Scanout {
                    state,
                    console: Some(console),
                    fbs: Vec::new(),
                    on_screen: 0,
                    cursor: None,
                    sets,
                    mode: None,
                })
            }
            Err(e) => {
                lines.push(format!(
                    "{}: modeset de la consola fallido: {}; la pantalla se queda como estaba",
                    tag,
                    e.describe()
                ));
                device.free_surface(console);
                None
            }
        }
    }

    /// Whether the LUTs the console modeset will use are in VRAM.
    ///
    /// On Turing every head reads its input and output LUTs from nvkms's
    /// default LUT surface while no client sets one, and nvkms fills that
    /// surface through the CPU, across BAR1. If those writes do not reach the
    /// VRAM the display reads, every pixel comes out black with the signal
    /// up, and there is no screen left to read the log on. So the surface is
    /// read back through the BAR0 window before anything is lit, rewritten
    /// there when it differs, and the modeset is skipped when that does not
    /// fix it. `false` leaves the screen as the firmware set it, and says why
    /// in `lines`.
    fn default_lut_reaches_vram(
        &self,
        device: &KmsDevice,
        gpu_id: u32,
        tag: &str,
        lines: &mut Vec<String>,
    ) -> bool {
        let Some(mut lut) = device.default_lut() else {
            return true;
        };
        let check = nvkms_os::check_vidmem(
            gpu_id,
            lut.h_client,
            lut.h_memory,
            lut.reference,
            lut.cpu,
            lut.size,
            true,
        );
        device.release_default_lut(&mut lut);
        match check {
            Ok(check) => {
                lines.push(format!("{}: {}", tag, check.describe()));
                if !check.usable() {
                    lines.push(format!(
                        "{}: sin modeset: con esa LUT la pantalla saldria negra o blanca; se queda \
                         como la dejo el firmware",
                        tag
                    ));
                }
                check.usable()
            }
            Err(status) => {
                lines.push(format!(
                    "{}: no se pudo leer la LUT por defecto en VRAM ({:#x}); sin modeset, la \
                     pantalla se queda como la dejo el firmware",
                    tag, status
                ));
                false
            }
        }
    }

    /// `page_flip` through nvkms: `None` when nvkms does not scan out this
    /// card, or cannot flip to `fb` (not in VRAM, or refused) -- then the
    /// console is put back on screen for the CPU or CE copy that takes the
    /// frame instead.
    pub(super) fn nvkms_page_flip(&self, fb: &NvidiaKmsFramebuffer) -> Option<bool> {
        let mut guard = self.nvkms_device.lock();
        let card = guard.as_mut()?;
        let device = &card.device;
        let scanout = card.scanout.as_mut()?;
        let in_vram = fb.h_memory != 0 && fb.vram_offset.is_some();
        if !in_vram || !self.nvkms_import(device, &mut scanout.fbs, fb) {
            // A tiled fb has no copy to fall back to, and the console stays.
            if fb.block_linear.is_some() {
                return Some(false);
            }
            if scanout.mode.is_some() {
                // The console does not fit a mode a client set.
                log_once(&OFF_MODE_COPY, || {
                    format!(
                        "{}: fb {} no se puede voltear y la consola no cabe en el modo {:?};                          ese frame se pierde",
                        self.bdf_tag(),
                        fb.id,
                        scanout.mode
                    )
                });
                return Some(false);
            }
            if scanout.flip_to(device, 0).is_err() {
                log_once(&CONSOLE_BACK_FAILED, || {
                    format!("{}: no se pudo volver a la consola", self.bdf_tag())
                });
            }
            return None;
        }
        match scanout.flip_to(device, fb.id) {
            Ok(()) => {
                let mut kms = self.kms_state.lock();
                kms.crtc_fb = fb.id;
                kms.plane_fb = fb.id;
                kms.last_vblank_us = now_us();
                drop(kms);
                log_once(&FIRST_FLIP, || {
                    format!(
                        "{}: primer flip de nvkms: fb {} {}x{} pitch {}",
                        self.bdf_tag(),
                        fb.id,
                        fb.width,
                        fb.height,
                        fb.pitch
                    )
                });
                Some(true)
            }
            Err(e) => {
                log_once(&FLIP_FAILED, || {
                    format!(
                        "{}: flip de nvkms fallido (fb {}): {}",
                        self.bdf_tag(),
                        fb.id,
                        e.describe()
                    )
                });
                Some(false)
            }
        }
    }

    /// The modes the monitor on screen can take, for the DRM connector:
    /// nvkms's validated modes of the first lit display still connected, or
    /// of the first connected one on a card no client has lit yet. Empty
    /// when nvkms does not hold this card, or without `nvidia.gem_vram`.
    pub(super) fn nvkms_modes(&self) -> Vec<KmsMode> {
        // A modeset scans out the compositor's buffer, and only one in VRAM
        // can be: without `nvidia.gem_vram` every modeset would be refused,
        // so the connector keeps listing the firmware's one mode.
        if !super::gem_vram::enabled() {
            return Vec::new();
        }
        let guard = self.nvkms_device.lock();
        let Some(card) = guard.as_ref() else {
            return Vec::new();
        };
        let connected = |handle: u32| {
            card.info.as_ref().is_none_or(|i| {
                i.displays()
                    .iter()
                    .any(|d| d.handle == handle && d.connected != 0)
            })
        };
        let lit = card
            .scanout
            .as_ref()
            .and_then(|s| s.sets.iter().map(|s| s.display).find(|&d| connected(d)));
        let first = || {
            card.info.as_ref().and_then(|i| {
                i.displays()
                    .iter()
                    .find(|d| d.connected != 0)
                    .map(|d| d.handle)
            })
        };
        let Some(display) = lit.or_else(first) else {
            return Vec::new();
        };
        nvkms::connector_modes(&card.device.modes(display))
    }

    /// A modeset through nvkms: every connected monitor of the card to
    /// `width`x`height` at the refresh rate closest to `refresh_hz`, all
    /// scanning out `fb` -- nvidia-drm's atomic commit of a new MODE_ID, with
    /// the card's monitors mirrored on its one DRM CRTC. A monitor lit
    /// already keeps its head, one plugged in since is lit, one unplugged
    /// since goes dark ([`nvkms::mode_plan`]). On a card with no scanout yet
    /// (no firmware console) this is the first modeset, which lights the
    /// card. `fb` has to be in VRAM: the console only fits the mode it was
    /// made at. `test_only` validates it and changes nothing. `None` when
    /// nvkms does not hold this card, or holds the console's card but could
    /// not show the console on it.
    pub(super) fn nvkms_set_mode(
        &self,
        width: u32,
        height: u32,
        refresh_hz: u32,
        fb: Option<&NvidiaKmsFramebuffer>,
        test_only: bool,
    ) -> Option<Result<(), String>> {
        let tag = self.bdf_tag();
        let mut guard = self.nvkms_device.lock();
        let card = guard.as_mut()?;
        if card.scanout.is_none() && !drives_own_display(self) {
            return None;
        }
        let NvkmsCard {
            device,
            scanout,
            info,
            ..
        } = card;
        let result = (|| {
            let Some(fb) = fb.filter(|fb| fb.h_memory != 0 && fb.vram_offset.is_some()) else {
                return Err(String::from(
                    "el fb no esta en VRAM (arranca con nvidia.gem_vram); nvkms solo escanea VRAM",
                ));
            };
            if fb.width < width || fb.height < height {
                return Err(format!("el fb {}x{} no cubre el modo", fb.width, fb.height));
            }
            let Some(info) = info.as_deref() else {
                return Err(String::from("nvkms no pudo consultar la tarjeta"));
            };
            let current = scanout.as_ref().map_or(&[][..], |s| s.sets.as_slice());
            let (sets, dark) = nvkms::mode_plan(
                info,
                |d| device.modes(d),
                current,
                width,
                height,
                refresh_hz,
            );
            if sets.is_empty() {
                return Err(if dark.is_empty() {
                    String::from("ningun monitor conectado")
                } else {
                    dark.join("; ")
                });
            }
            if !test_only {
                for line in &dark {
                    crate::klog_info!("[nvkms] {}: {}", tag, line);
                }
            }
            match scanout {
                Some(scanout) => {
                    if !self.nvkms_import(device, &mut scanout.fbs, fb) {
                        return Err(String::from("nvkms no acepta el fb como superficie"));
                    }
                    let surface = scanout
                        .surface_of(fb.id)
                        .ok_or_else(|| String::from("sin superficie"))?;
                    let start = now_us();
                    loop {
                        match device.set_mode(&scanout.state, surface, &sets, test_only) {
                            Err(ShowError::FlipPending)
                                if now_us().wrapping_sub(start) < FLIP_WAIT_US =>
                            {
                                core::hint::spin_loop()
                            }
                            Err(e) => return Err(String::from(e.describe())),
                            Ok(()) => break,
                        }
                    }
                    if test_only {
                        return Ok(sets.len());
                    }
                    let console_size = super::BOOT_FB_INFO
                        .lock()
                        .is_some_and(|c| c.width == width && c.height == height);
                    scanout.on_screen = fb.id;
                    scanout.mode = (!(console_size && scanout.console.is_some()))
                        .then_some((width, height, refresh_hz));
                    scanout.sets = sets.clone();
                }
                None => {
                    let mut fbs = Vec::new();
                    if !self.nvkms_import(device, &mut fbs, fb) {
                        return Err(String::from("nvkms no acepta el fb como superficie"));
                    }
                    if test_only {
                        // Nothing is on screen to test against: the plan and
                        // the surface are what can be checked.
                        for (_, surface) in fbs {
                            if let Some(surface) = surface {
                                device.free_surface(surface);
                            }
                        }
                        return Ok(sets.len());
                    }
                    let shown = match fbs.first().and_then(|(_, s)| s.as_ref()) {
                        Some(surface) => device.show_console(surface, &sets, info.num_heads),
                        None => Err(ShowError::BadRequest),
                    };
                    let state = match shown {
                        Ok(state) => state,
                        Err(e) => {
                            for (_, surface) in fbs {
                                if let Some(surface) = surface {
                                    device.free_surface(surface);
                                }
                            }
                            return Err(String::from(e.describe()));
                        }
                    };
                    *scanout = Some(Scanout {
                        state,
                        console: None,
                        fbs,
                        on_screen: fb.id,
                        cursor: None,
                        sets: sets.clone(),
                        mode: Some((width, height, refresh_hz)),
                    });
                }
            }
            if let Some(set) = sets.first() {
                self.vblank_head.store(set.head, Ordering::Release);
            }
            let mut kms = self.kms_state.lock();
            kms.crtc_fb = fb.id;
            kms.plane_fb = fb.id;
            kms.last_vblank_us = now_us();
            Ok(sets.len())
        })();
        if !test_only {
            match &result {
                Ok(heads) => crate::klog_info!(
                    "[nvkms] {}: modeset hecho: {}x{} a {} Hz en {} cabeza(s)",
                    tag,
                    width,
                    height,
                    refresh_hz,
                    heads
                ),
                Err(why) => crate::klog_warn!(
                    "[nvkms] {}: modeset a {}x{} a {} Hz rechazado: {}; la pantalla sigue igual",
                    tag,
                    width,
                    height,
                    refresh_hz,
                    why
                ),
            }
        }
        Some(result.map(|_| ()))
    }

    /// Whether nvkms takes the fb `fb_id` as a surface, made now: a
    /// block-linear fb it refuses is refused at `ADDFB2`, so the compositor
    /// goes back to linear before a frame is lost.
    pub(super) fn nvkms_takes_fb(&self, fb_id: u32) -> bool {
        let Some(fb) = self.kms_fb(fb_id) else {
            return false;
        };
        if fb.h_memory == 0 || fb.vram_offset.is_none() {
            return false;
        }
        let mut guard = self.nvkms_device.lock();
        let Some(card) = guard.as_mut() else {
            return false;
        };
        let device = &card.device;
        let Some(scanout) = card.scanout.as_mut() else {
            return false;
        };
        self.nvkms_import(device, &mut scanout.fbs, &fb)
    }

    /// Make sure `fb` has its nvkms surface: its GEM duped into kapi's RM
    /// client. Whether it has one.
    fn nvkms_import(
        &self,
        device: &KmsDevice,
        fbs: &mut Vec<(u32, Option<KmsSurface>)>,
        fb: &NvidiaKmsFramebuffer,
    ) -> bool {
        if let Some((_, surface)) = fbs.iter().find(|(id, _)| *id == fb.id) {
            return surface.is_some();
        }
        let dev = *self.rm_device_instance.lock();
        let owner_ctx = self
            .nouveau_gem
            .lock()
            .iter()
            .find(|o| o.handle == fb.handle_id)
            .map(|o| o.owner_ctx);
        let surface = dev
            .zip(owner_ctx)
            .and_then(|(dev, ctx)| nvidia_rm_sys::rm_init::gem_client(dev, ctx))
            .and_then(|h_client| {
                device.import_surface(
                    h_client,
                    fb.h_memory,
                    fb.size as u64,
                    fb.width,
                    fb.height,
                    fb.pitch,
                    fb.block_linear,
                )
            });
        if surface.is_none() {
            crate::klog_warn!(
                "[nvkms] {}: nvkms no acepta el fb {} ({}x{} pitch {}, hMem {:#x}, {}) como \
                 superficie; {}",
                self.bdf_tag(),
                fb.id,
                fb.width,
                fb.height,
                fb.pitch,
                fb.h_memory,
                layout_name(fb.block_linear),
                if fb.block_linear.is_some() {
                    "ADDFB2 lo rechaza y el compositor vuelve a lineal"
                } else {
                    "ese fb se copia a la consola"
                }
            );
        } else if let Some(h) = fb.block_linear {
            log_once(&FIRST_BLOCK_LINEAR, || {
                format!(
                    "{}: primera superficie block-linear (bloques de {} GOBs, fb {} {}x{} pitch {}): \
                     nvkms la escanea sin copia",
                    self.bdf_tag(),
                    1u32 << h,
                    fb.id,
                    fb.width,
                    fb.height,
                    fb.pitch
                )
            });
        }
        let ok = surface.is_some();
        fbs.push((fb.id, surface));
        ok
    }

    /// The fbs in `ids` are gone: free their surfaces, after putting the
    /// console back if one of them is on screen and letting that flip land.
    pub(super) fn nvkms_forget_fbs(&self, ids: &[u32]) {
        if ids.is_empty() {
            return;
        }
        let mut guard = self.nvkms_device.lock();
        let Some(card) = guard.as_mut() else {
            return;
        };
        let device = &card.device;
        let Some(scanout) = card.scanout.as_mut() else {
            return;
        };
        // Away from the console's mode there is nothing to put back: the
        // surface on screen stays until the next flip or modeset.
        if ids.contains(&scanout.on_screen)
            && (scanout.mode.is_some()
                || !(scanout.flip_to(device, 0).is_ok() && scanout.wait_landed(device)))
        {
            // Still on screen: keep the surface (and the memory it holds a
            // reference on) rather than have nvkms scan out freed VRAM.
            crate::klog_warn!(
                "[nvkms] {}: el fb {} sigue en pantalla; su superficie se queda",
                self.bdf_tag(),
                scanout.on_screen
            );
        }
        let mut i = 0;
        while i < scanout.fbs.len() {
            let (id, _) = scanout.fbs[i];
            if ids.contains(&id) && id != scanout.on_screen {
                if let (_, Some(surface)) = scanout.fbs.swap_remove(i) {
                    device.free_surface(surface);
                }
            } else {
                i += 1;
            }
        }
    }

    /// `hw_cursor_set` on a card nvkms holds: the image into nvkms's cursor
    /// plane, at the pointer's last position.
    pub(super) fn nvkms_cursor_set(&self, argb: &[u32], w: u32, h: u32) -> bool {
        let Some(image) = nvkms::cursor_image(argb, w, h) else {
            return false;
        };
        let mut guard = self.nvkms_device.lock();
        let Some(card) = guard.as_mut() else {
            return false;
        };
        let device = &card.device;
        let Some(scanout) = card.scanout.as_mut() else {
            return false;
        };
        if scanout.cursor.is_none() {
            let Some((surface, h_client, h_memory)) = device.cursor_surface() else {
                log_once(&CURSOR_FAILED, || {
                    format!(
                        "{}: sin superficie para el cursor de nvkms; cursor software",
                        self.bdf_tag()
                    )
                });
                return false;
            };
            scanout.cursor = Some(Cursor {
                surface,
                h_client,
                h_memory,
                x: 0,
                y: 0,
                visible: false,
            });
        }
        let Some(cursor) = scanout.cursor.as_mut() else {
            return false;
        };
        if nvidia_rm_sys::rm_init::write_memory(cursor.h_client, cursor.h_memory, 0, &image) != 0 {
            log_once(&CURSOR_FAILED, || {
                format!(
                    "{}: no se pudo escribir el cursor de nvkms; cursor software",
                    self.bdf_tag()
                )
            });
            return false;
        }
        let shown = device
            .cursor(
                &scanout.state,
                Some(&cursor.surface),
                cursor.x,
                cursor.y,
                true,
            )
            .is_ok();
        cursor.visible = shown;
        shown
    }

    /// `hw_cursor_move` on a card nvkms holds; `None` when it does not.
    pub(super) fn nvkms_cursor_move(&self, x: i32, y: i32) -> Option<bool> {
        let mut guard = self.nvkms_device.lock();
        let card = guard.as_mut()?;
        let scanout = card.scanout.as_mut()?;
        let cursor = scanout.cursor.as_mut()?;
        cursor.x = x;
        cursor.y = y;
        if !cursor.visible {
            return Some(true);
        }
        Some(
            card.device
                .cursor(&scanout.state, Some(&cursor.surface), x, y, false)
                .is_ok(),
        )
    }

    /// `hw_cursor_hide` on a card nvkms holds; `None` when it does not.
    pub(super) fn nvkms_cursor_hide(&self) -> Option<bool> {
        let mut guard = self.nvkms_device.lock();
        let card = guard.as_mut()?;
        let scanout = card.scanout.as_mut()?;
        let cursor = scanout.cursor.as_mut()?;
        cursor.visible = false;
        Some(
            card.device
                .cursor(&scanout.state, None, cursor.x, cursor.y, false)
                .is_ok(),
        )
    }

    /// One pass of the output poll on this card (Linux's
    /// `output_poll_execute` plus nvidia-drm's hotplug event): whether a
    /// monitor came or went since the last pass, so the DRM layer sends the
    /// hotplug uevent. A pass looks at nvkms's hotplug events, and every
    /// [`RM_DETECT_EVERY`] passes asks the RM to detect again, for a
    /// monitor whose event never arrived. On a change the card is probed
    /// again (the modes and EDID nvkms lists) and the RM's connector answer
    /// is read again, so `GETCONNECTOR` tells the new monitor. Also true
    /// once when nvkms has just taken a card that lights its own monitors:
    /// its connectors read disconnected until then.
    pub(super) fn nvkms_poll_connectors(&self) -> bool {
        if !REQUESTED.load(Ordering::Relaxed) {
            return false;
        }
        let instance = *self.rm_device_instance.lock();
        let mut guard = self.nvkms_device.lock();
        let Some(card) = guard.as_mut() else {
            return false;
        };
        let mut changed = false;
        if !card.poll.announced {
            card.poll.announced = true;
            changed = drives_own_display(self);
        }
        card.poll.passes = card.poll.passes.wrapping_add(1);
        let hotplugs = nvkms::hotplug_count();
        let mut probe = hotplugs != card.hotplugs;
        if card.poll.passes % RM_DETECT_EVERY == 0 {
            if let Some(mask) = instance.and_then(|i| nvidia_rm_sys::rm_init::connect_state(i).ok())
            {
                probe |= card.poll.rm_connected.is_some_and(|was| was != mask);
                card.poll.rm_connected = Some(mask);
            }
        }
        if !probe {
            return changed;
        }
        card.hotplugs = hotplugs;
        let before = monitors(card.info.as_deref());
        if let Some(info) = card.device.probe() {
            card.info = Some(info);
        }
        let after = monitors(card.info.as_deref());
        drop(guard);
        if before == after {
            return changed;
        }
        crate::klog_info!(
            "[nvkms] {}: los monitores cambiaron: {} conectado(s) antes, {} ahora",
            self.bdf_tag(),
            before.len(),
            after.len()
        );
        if let Some(instance) = instance {
            self.rm_display_refresh(instance);
        }
        true
    }

    /// Read the RM's connectors and EDID again after a hotplug, replacing
    /// the answer `rm_display_state` kept for the boot. On a failed read the
    /// old answer stays, with only its connected mask brought up to date.
    fn rm_display_refresh(&self, instance: u32) {
        use nvidia_rm_sys::rm_init;
        rm_init::edid_forget(instance);
        let fresh = rm_init::edid(instance)
            .ok()
            .filter(|d| d.supported_status == 0 && d.display_mask != 0);
        let mut snap = self.rm_display_snap.lock();
        match (fresh, snap.as_mut()) {
            (Some(d), _) => *snap = Some((instance, d)),
            (None, Some((_, d))) => {
                if let Ok(mask) = rm_init::connect_state(instance) {
                    d.connected_mask = mask;
                }
            }
            (None, None) => {}
        }
    }

    /// The connectors of a card that lights its own monitors read
    /// disconnected until nvkms holds it: before that no modeset can light
    /// them.
    pub(super) fn nvkms_display_pending(&self) -> bool {
        drives_own_display(self) && !self.nvkms_owns_display()
    }

    /// Run nvkms's due timers, and say how many microseconds until the next
    /// one (`u64::MAX` for none queued). `None` when this card is not nvkms's.
    pub(super) fn nvkms_service(&self) -> Option<u64> {
        if !self.nvkms_owns_display() {
            return None;
        }
        Some(match nvkms_os::service_now() {
            Some(deadline) => deadline.saturating_sub(nvkms_os::nvkms_get_usec()),
            None => u64::MAX,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvkms_claims_an_untried_or_failed_ladder() {
        let s = AtomicU8::new(0);
        assert_eq!(claim(&s), Ok(0));
        assert_eq!(s.load(Ordering::Relaxed), NVKMS_OWNS);
        let s = AtomicU8::new(2);
        assert_eq!(claim(&s), Ok(2));
    }

    #[test]
    fn nvkms_never_takes_a_core_channel_the_ladder_holds() {
        let s = AtomicU8::new(1);
        assert_eq!(claim(&s), Err(1));
        assert_eq!(s.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_console_depth_comes_from_pitch_and_width() {
        assert_eq!(console_depth(1920, 7680), 32);
        assert_eq!(console_depth(2048, 8192), 32);
        // A padded pitch is still 32-bit.
        assert_eq!(console_depth(1366, 5504), 32);
        assert_eq!(console_depth(1024, 2048), 16);
        assert_eq!(console_depth(0, 7680), 0);
    }
}
