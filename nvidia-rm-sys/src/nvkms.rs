//! nvidia-modeset (nvkms): the modeset, DisplayPort and HDMI code Linux's
//! `nvidia-modeset.ko` is built from, compiled by `build.rs` from the same
//! submodule as the RM. See `docs/README-nvkms-port.md`.
//!
//! Eclipse drives it the way `nvidia-drm` does in Linux, through the kapi
//! function table. The calls themselves are made in C
//! (`vendor/eclipse_nvkms_drv.c`), because kapi's structs are too large and
//! too full of bitfields to mirror here; this module gets back the flat
//! [`KmsInfo`] and turns it into something to log.
//!
//! The first modeset keeps the firmware console on screen
//! ([`KmsDevice::show_console`]): which display gets which head and mode is
//! decided here ([`console_plan`]), where it can be tested. After it, the
//! compositor's buffers are flipped to ([`KmsDevice::flip`]).
//!
//! Loaded only with `nvidia.nvkms` ([`crate::nvkms_os::module_load`]).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::NonNull;

#[cfg(target_os = "none")]
extern "C" {
    /// `nvKmsKapiGetFunctionsTable` (kapi/interface/nvkms-kapi.h). Only its
    /// address is taken here.
    fn nvKmsKapiGetFunctionsTable(funcs_table: *mut c_void) -> u8;
    fn eclipse_kms_alloc_device(gpu_id: u32) -> *mut c_void;
    fn eclipse_kms_free_device(device: *mut c_void);
    fn eclipse_kms_probe(device: *mut c_void, info: *mut KmsInfo) -> u8;
    fn eclipse_kms_default_lut(device: *mut c_void, out: *mut KmsLut) -> u8;
    fn eclipse_kms_mode(device: *mut c_void, display: u32, index: u32, out: *mut KmsMode) -> i32;
    fn eclipse_kms_console_surface(
        device: *mut c_void,
        width: u32,
        height: u32,
        pitch: u32,
        depth: u32,
    ) -> *mut c_void;
    fn eclipse_kms_import_surface(
        device: *mut c_void,
        h_src_client: u32,
        h_src_memory: u32,
        size: u64,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> *mut c_void;
    fn eclipse_kms_surface_free(device: *mut c_void, surface: *mut c_void);
    fn eclipse_kms_show_console(
        device: *mut c_void,
        console: *const c_void,
        sets: *const KmsHeadSet,
        num_sets: u32,
        num_heads: u32,
        out: *mut *mut c_void,
    ) -> i32;
    fn eclipse_kms_flip(device: *mut c_void, scanout: *mut c_void, surface: *const c_void) -> i32;
    fn eclipse_kms_flip_pending(device: *mut c_void, scanout: *const c_void) -> i32;
    fn eclipse_kms_cursor_surface(
        device: *mut c_void,
        h_client: *mut u32,
        h_memory: *mut u32,
    ) -> *mut c_void;
    fn eclipse_kms_cursor(
        device: *mut c_void,
        scanout: *mut c_void,
        surface: *const c_void,
        x: i32,
        y: i32,
        surface_changed: u8,
    ) -> i32;
}

/// Whether nvkms is in this kernel image.
///
/// Takes the address of kapi's entry point without calling it. That reference
/// is what makes the linker pull nvkms out of its archive -- and with it every
/// object kapi reaches -- so an unresolved symbol anywhere in nvkms fails the
/// kernel link instead of hiding in an archive nobody looked into. Hosted
/// builds compile no NVIDIA C and answer `false`.
pub fn linked() -> bool {
    #[cfg(target_os = "none")]
    {
        let entry: unsafe extern "C" fn(*mut c_void) -> u8 = nvKmsKapiGetFunctionsTable;
        core::hint::black_box(entry as usize) != 0
    }
    #[cfg(not(target_os = "none"))]
    {
        false
    }
}

/// Displays [`KmsInfo`] describes; nvkms may count more.
pub const MAX_DISPLAYS: usize = 8;

/// One display as nvkms sees it: `struct EclipseKmsDisplay`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KmsDisplay {
    pub handle: u32,
    pub connector: u32,
    /// `NvKmsConnectorType`.
    pub connector_type: u32,
    /// `NvKmsConnectorSignalFormat`.
    pub signal_format: u32,
    pub physical_index: u32,
    pub head_mask: u32,
    /// Modes nvkms validated for this display.
    pub num_modes: u32,
    pub preferred_width: u32,
    pub preferred_height: u32,
    pub preferred_refresh_millihz: u32,
    pub preferred_pixel_clock_khz: u32,
    /// The whole EDID nvkms read; [`Self::edid`] holds its base block.
    pub edid_size: u32,
    pub connected: u8,
    pub internal: u8,
    pub is_dp_mst: u8,
    pub vrr_supported: u8,
    pub edid: [u8; 128],
}

/// What nvkms reports for one GPU: `struct EclipseKmsInfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KmsInfo {
    pub num_heads: u32,
    pub num_connectors: u32,
    pub num_displays: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub max_cursor: u32,
    pub pitch_alignment: u32,
    pub has_video_memory: u32,
    /// The firmware console nvkms imported as a surface, 0 for none.
    pub vt_fb_base: u64,
    pub vt_fb_size: u64,
    pub displays: [KmsDisplay; MAX_DISPLAYS],
}

// The same sizes the C side asserts.
const _: () = assert!(core::mem::size_of::<KmsDisplay>() == 4 * 12 + 4 + 128);
const _: () =
    assert!(core::mem::size_of::<KmsInfo>() == 8 * 4 + 2 * 8 + MAX_DISPLAYS * (4 * 12 + 4 + 128));

impl KmsInfo {
    pub const fn zeroed() -> Self {
        const D: KmsDisplay = KmsDisplay {
            handle: 0,
            connector: 0,
            connector_type: 0,
            signal_format: 0,
            physical_index: 0,
            head_mask: 0,
            num_modes: 0,
            preferred_width: 0,
            preferred_height: 0,
            preferred_refresh_millihz: 0,
            preferred_pixel_clock_khz: 0,
            edid_size: 0,
            connected: 0,
            internal: 0,
            is_dp_mst: 0,
            vrr_supported: 0,
            edid: [0; 128],
        };
        Self {
            num_heads: 0,
            num_connectors: 0,
            num_displays: 0,
            max_width: 0,
            max_height: 0,
            max_cursor: 0,
            pitch_alignment: 0,
            has_video_memory: 0,
            vt_fb_base: 0,
            vt_fb_size: 0,
            displays: [D; MAX_DISPLAYS],
        }
    }

    /// The displays that were described, connected or not.
    pub fn displays(&self) -> &[KmsDisplay] {
        &self.displays[..(self.num_displays as usize).min(MAX_DISPLAYS)]
    }
}

/// An nvkms device: kapi's `struct NvKmsKapiDevice`, one per GPU, the way
/// `nv_drm_device` holds one in Linux.
pub struct KmsDevice(
    // Only handed back to the C side, which a hosted build does not compile.
    #[cfg_attr(not(target_os = "none"), allow(dead_code))] NonNull<c_void>,
);

// The pointer is only ever handed back to kapi, which serialises every entry
// under nvkms_lock.
unsafe impl Send for KmsDevice {}
unsafe impl Sync for KmsDevice {}

impl KmsDevice {
    /// kapi's `allocateDevice` for the GPU the RM calls `gpu_id`. Allocates
    /// nvkms's display state for it, core channel included, without changing
    /// what is on its screen. `None` when kapi refused (its reason is in the
    /// log); always `None` in a hosted build.
    pub fn alloc(gpu_id: u32) -> Option<Self> {
        #[cfg(target_os = "none")]
        {
            NonNull::new(unsafe { eclipse_kms_alloc_device(gpu_id) }).map(Self)
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = gpu_id;
            None
        }
    }

    /// Heads, connectors, displays, EDID and modes, as nvkms sees them now.
    pub fn probe(&self) -> Option<Box<KmsInfo>> {
        let mut info = Box::new(KmsInfo::zeroed());
        #[cfg(target_os = "none")]
        {
            if unsafe { eclipse_kms_probe(self.0.as_ptr(), &mut *info) } == 0 {
                return None;
            }
            Some(info)
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = &mut info;
            None
        }
    }

    /// The default LUT surface (Turing and later): the input and output LUTs
    /// every head uses while no client sets its own, which the console
    /// modeset does not. `None` when the device has none.
    pub fn default_lut(&self) -> Option<KmsLut> {
        let mut lut = KmsLut::default();
        #[cfg(target_os = "none")]
        {
            if unsafe { eclipse_kms_default_lut(self.0.as_ptr(), &mut lut) } == 0 {
                return None;
            }
            Some(lut)
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = &mut lut;
            None
        }
    }

    /// Every mode nvkms lists for `display`, valid or not: a mode's position
    /// here is the index [`KmsHeadSet::mode_index`] names.
    pub fn modes(&self, display: u32) -> Vec<KmsMode> {
        let mut modes = Vec::new();
        #[cfg(target_os = "none")]
        for index in 0..MAX_MODES as u32 {
            let mut mode = KmsMode::default();
            let more = unsafe { eclipse_kms_mode(self.0.as_ptr(), display, index, &mut mode) };
            if more < 0 {
                break;
            }
            modes.push(mode);
            if more == 0 {
                break;
            }
        }
        #[cfg(not(target_os = "none"))]
        let _ = (display, &mut modes);
        modes
    }

    /// The firmware console (the VRAM the RM reserved for it) as a surface
    /// nvkms can scan out. `None` when the RM or kapi refused; the reason is
    /// in the NVKMS log.
    pub fn console_surface(
        &self,
        width: u32,
        height: u32,
        pitch: u32,
        depth: u32,
    ) -> Option<KmsSurface> {
        #[cfg(target_os = "none")]
        {
            NonNull::new(unsafe {
                eclipse_kms_console_surface(self.0.as_ptr(), width, height, pitch, depth)
            })
            .map(KmsSurface)
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = (width, height, pitch, depth);
            None
        }
    }

    /// A compositor's VRAM buffer (an XRGB8888 pitch-linear GEM, the RM
    /// handle `h_memory` in the client `h_client`) as a surface to flip to.
    /// `None` when the RM or kapi refused; the reason is in the NVKMS log.
    pub fn import_surface(
        &self,
        h_client: u32,
        h_memory: u32,
        size: u64,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> Option<KmsSurface> {
        #[cfg(target_os = "none")]
        {
            NonNull::new(unsafe {
                eclipse_kms_import_surface(
                    self.0.as_ptr(),
                    h_client,
                    h_memory,
                    size,
                    width,
                    height,
                    pitch,
                )
            })
            .map(KmsSurface)
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = (h_client, h_memory, size, width, height, pitch);
            None
        }
    }

    /// The first modeset: light each head in `sets` with the console, and
    /// darken the device's other heads. Takes modeset ownership, and gives it
    /// back on failure. The configuration it leaves on screen is what
    /// [`Self::flip`] changes.
    pub fn show_console(
        &self,
        console: &KmsSurface,
        sets: &[KmsHeadSet],
        num_heads: u32,
    ) -> Result<KmsScanout, ShowError> {
        #[cfg(target_os = "none")]
        {
            let mut out = core::ptr::null_mut();
            let code = unsafe {
                eclipse_kms_show_console(
                    self.0.as_ptr(),
                    console.0.as_ptr(),
                    sets.as_ptr(),
                    sets.len() as u32,
                    num_heads,
                    &mut out,
                )
            };
            ShowError::from_code(code)?;
            NonNull::new(out)
                .map(KmsScanout)
                .ok_or(ShowError::BadRequest)
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = (console, sets, num_heads);
            Err(ShowError::BadRequest)
        }
    }

    /// Flip every lit head to `surface` at the next vblank. Returns once the
    /// flip is queued; [`ShowError::FlipPending`] while the previous one has
    /// not landed, with nothing changed.
    pub fn flip(&self, scanout: &KmsScanout, surface: &KmsSurface) -> Result<(), ShowError> {
        #[cfg(target_os = "none")]
        {
            ShowError::from_code(unsafe {
                eclipse_kms_flip(self.0.as_ptr(), scanout.0.as_ptr(), surface.0.as_ptr())
            })
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = (scanout, surface);
            Err(ShowError::BadRequest)
        }
    }

    /// Whether a flip has yet to land on any lit head; `None` when kapi
    /// could not say.
    pub fn flip_pending(&self, scanout: &KmsScanout) -> Option<bool> {
        #[cfg(target_os = "none")]
        {
            match unsafe { eclipse_kms_flip_pending(self.0.as_ptr(), scanout.0.as_ptr()) } {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            }
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = scanout;
            None
        }
    }

    /// The cursor plane's surface: [`CURSOR_SIZE`] square, A8R8G8B8, in
    /// video memory, with the RM client and handle its image is written
    /// through ([`crate::rm_init::write_memory`]).
    pub fn cursor_surface(&self) -> Option<(KmsSurface, u32, u32)> {
        #[cfg(target_os = "none")]
        {
            let (mut h_client, mut h_memory) = (0, 0);
            let s = unsafe {
                eclipse_kms_cursor_surface(self.0.as_ptr(), &mut h_client, &mut h_memory)
            };
            NonNull::new(s).map(|s| (KmsSurface(s), h_client, h_memory))
        }
        #[cfg(not(target_os = "none"))]
        None
    }

    /// The cursor plane on every lit head: `surface` with its top-left at
    /// `(x, y)`, or off for `None`. `changed` says the image in `surface` is
    /// new. Never waits for a pending flip.
    pub fn cursor(
        &self,
        scanout: &KmsScanout,
        surface: Option<&KmsSurface>,
        x: i32,
        y: i32,
        changed: bool,
    ) -> Result<(), ShowError> {
        #[cfg(target_os = "none")]
        {
            let s = surface.map_or(core::ptr::null(), |s| s.0.as_ptr() as *const c_void);
            ShowError::from_code(unsafe {
                eclipse_kms_cursor(self.0.as_ptr(), scanout.0.as_ptr(), s, x, y, changed as u8)
            })
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = (scanout, surface, x, y, changed);
            Err(ShowError::BadRequest)
        }
    }

    /// Destroy a surface. It must not be on screen.
    pub fn free_surface(&self, surface: KmsSurface) {
        #[cfg(target_os = "none")]
        unsafe {
            eclipse_kms_surface_free(self.0.as_ptr(), surface.0.as_ptr())
        };
        #[cfg(not(target_os = "none"))]
        let _ = surface;
    }

    /// kapi's `freeDevice`.
    pub fn free(self) {
        #[cfg(target_os = "none")]
        unsafe {
            eclipse_kms_free_device(self.0.as_ptr())
        };
    }
}

/// A kapi surface and its memory: `struct EclipseKmsSurface`.
pub struct KmsSurface(#[cfg_attr(not(target_os = "none"), allow(dead_code))] NonNull<c_void>);

/// The configuration nvkms scans out since the console modeset:
/// `struct EclipseKmsScanout`. Lives as long as the device.
pub struct KmsScanout(#[cfg_attr(not(target_os = "none"), allow(dead_code))] NonNull<c_void>);

// Like `KmsDevice`, only ever handed back to kapi.
unsafe impl Send for KmsSurface {}
unsafe impl Sync for KmsSurface {}
unsafe impl Send for KmsScanout {}
unsafe impl Sync for KmsScanout {}

/// The cursor plane's width and height: `ECLIPSE_KMS_CURSOR`, the size the
/// driver advertises.
pub const CURSOR_SIZE: u32 = 64;

/// A cursor image of `w`x`h` premultiplied ARGB pixels as the bytes of a
/// [`CURSOR_SIZE`] square surface: the image at the top left, transparent
/// around it. `None` for an image that does not fit.
pub fn cursor_image(argb: &[u32], w: u32, h: u32) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || w > CURSOR_SIZE || h > CURSOR_SIZE || argb.len() < (w * h) as usize {
        return None;
    }
    let side = CURSOR_SIZE as usize;
    let mut out = alloc::vec![0u8; side * side * 4];
    for row in 0..h as usize {
        for col in 0..w as usize {
            let at = (row * side + col) * 4;
            out[at..at + 4].copy_from_slice(&argb[row * w as usize + col].to_le_bytes());
        }
    }
    Some(out)
}

/// Modes [`KmsDevice::modes`] asks about, as the probe does.
pub const MAX_MODES: usize = 256;

/// One mode of a display: `struct EclipseKmsMode`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KmsMode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub pixel_clock_khz: u32,
    pub valid: u8,
    pub preferred: u8,
    pub interlaced: u8,
    pub pad: u8,
}

const _: () = assert!(core::mem::size_of::<KmsMode>() == 4 * 4 + 4);

/// The device's default LUT surface: `struct EclipseKmsLut`. Where nvkms
/// writes it (`cpu`, a BAR1 mapping) and the RM object that holds it, for
/// [`crate::nvkms_os::check_vidmem`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KmsLut {
    pub cpu: u64,
    pub h_client: u32,
    pub h_memory: u32,
    pub size: u32,
    pub pad: u32,
}

const _: () = assert!(core::mem::size_of::<KmsLut>() == 8 + 4 * 4);

/// One head the console modeset lights: `struct EclipseKmsHeadSet`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KmsHeadSet {
    pub head: u32,
    pub display: u32,
    /// Index into [`KmsDevice::modes`] for `display`.
    pub mode_index: u32,
}

/// Why [`KmsDevice::show_console`] or [`KmsDevice::flip`] left the screen as
/// it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShowError {
    BadRequest,
    NoOwnership,
    ModeGone,
    Refused,
    CommitFailed,
    FlipPending,
}

impl ShowError {
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    fn from_code(code: i32) -> Result<(), Self> {
        match code {
            0 => Ok(()),
            2 => Err(Self::NoOwnership),
            3 => Err(Self::ModeGone),
            4 => Err(Self::Refused),
            5 => Err(Self::CommitFailed),
            6 => Err(Self::FlipPending),
            _ => Err(Self::BadRequest),
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::BadRequest => "peticion mal formada",
            Self::NoOwnership => "nvkms no dio la propiedad del modeset",
            Self::ModeGone => "el modo ya no es valido",
            Self::Refused => "nvkms rechazo la configuracion al validarla",
            Self::CommitFailed => "el commit del modeset fallo",
            Self::FlipPending => "el flip anterior aun no ha llegado a la pantalla",
        }
    }
}

/// The mode to show a `width`x`height` console at: one of exactly that size,
/// so the console is shown unscaled, the way the firmware set it. The
/// preferred mode if it is that size -- the one the UEFI picks -- else the
/// one closest to 60 Hz. Never an interlaced mode.
pub fn console_mode(modes: &[KmsMode], width: u32, height: u32) -> Option<usize> {
    let fits =
        |m: &KmsMode| m.valid != 0 && m.interlaced == 0 && m.width == width && m.height == height;
    if let Some(i) = modes.iter().position(|m| fits(m) && m.preferred != 0) {
        return Some(i);
    }
    modes
        .iter()
        .enumerate()
        .filter(|(_, m)| fits(m))
        .min_by_key(|(_, m)| m.refresh_millihz.abs_diff(60_000))
        .map(|(i, _)| i)
}

/// The console modeset for one GPU: which head and mode each connected
/// display gets, and one line for each display left dark. Displays take the
/// lowest free head they can be driven from, in nvkms's order, the way the
/// firmware clones its console onto every monitor.
pub fn console_plan(
    info: &KmsInfo,
    mut modes_of: impl FnMut(u32) -> Vec<KmsMode>,
    width: u32,
    height: u32,
) -> (Vec<KmsHeadSet>, Vec<String>) {
    let mut sets: Vec<KmsHeadSet> = Vec::new();
    let mut dark = Vec::new();
    let heads = if info.num_heads >= 32 {
        u32::MAX
    } else {
        (1u32 << info.num_heads) - 1
    };
    for d in info.displays().iter().filter(|d| d.connected != 0) {
        let modes = modes_of(d.handle);
        let Some(mode_index) = console_mode(&modes, width, height) else {
            dark.push(format!(
                "pantalla {:#x}: ningun modo de {}x{} para la consola; se queda apagada",
                d.handle, width, height
            ));
            continue;
        };
        let used = sets.iter().fold(0u32, |m, s| m | (1 << s.head));
        let free = d.head_mask & heads & !used;
        if free == 0 {
            dark.push(format!(
                "pantalla {:#x}: sin cabeza libre (cabezas {:#x}); se queda apagada",
                d.handle, d.head_mask
            ));
            continue;
        }
        sets.push(KmsHeadSet {
            head: free.trailing_zeros(),
            display: d.handle,
            mode_index: mode_index as u32,
        });
    }
    (sets, dark)
}

/// `NvKmsConnectorTypeString` of nvkms-api-types.h.
pub fn connector_type_name(t: u32) -> &'static str {
    match t {
        0 => "DP",
        1 => "VGA",
        2 => "DVI-I",
        3 => "DVI-D",
        4 => "ADC",
        5 => "LVDS",
        6 => "HDMI",
        7 => "USB-C",
        8 => "DSI",
        9 => "DP-SERIALIZER",
        _ => "desconocido",
    }
}

/// `NvKmsConnectorSignalFormat`.
pub fn signal_format_name(f: u32) -> &'static str {
    match f {
        0 => "VGA",
        1 => "LVDS",
        2 => "TMDS",
        3 => "DP",
        4 => "DSI",
        _ => "desconocido",
    }
}

/// The monitor name from an EDID base block: the 0xFC display descriptor.
pub fn edid_monitor_name(edid: &[u8; 128]) -> Option<String> {
    if edid[..8] != [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00] {
        return None;
    }
    for d in edid[54..126].as_chunks::<18>().0 {
        if d[0] == 0 && d[1] == 0 && d[3] == 0xfc {
            let text: String = d[5..]
                .iter()
                .take_while(|&&b| b != b'\n')
                .map(|&b| b as char)
                .collect();
            let name = text.trim_end();
            if !name.is_empty() {
                return Some(String::from(name));
            }
        }
    }
    None
}

/// The log lines for one GPU's probe: a summary, then one line per display.
pub fn describe(info: &KmsInfo) -> Vec<String> {
    let mut lines = Vec::new();
    let console = if info.vt_fb_base != 0 {
        format!(
            "consola importada {:#x}+{:#x}",
            info.vt_fb_base, info.vt_fb_size
        )
    } else {
        String::from("sin consola importada")
    };
    lines.push(format!(
        "{} cabezas, {} conectores, {} pantallas; max {}x{}, cursor {}, pitch /{}; {}",
        info.num_heads,
        info.num_connectors,
        info.num_displays,
        info.max_width,
        info.max_height,
        info.max_cursor,
        info.pitch_alignment,
        console,
    ));
    for d in info.displays() {
        let mut line = format!(
            "pantalla {:#x}: {} ({}) conector {} cabezas {:#x}",
            d.handle,
            connector_type_name(d.connector_type),
            signal_format_name(d.signal_format),
            d.physical_index,
            d.head_mask,
        );
        if d.connected == 0 {
            line.push_str(" -- desconectada");
        } else {
            let name = edid_monitor_name(&d.edid).unwrap_or_else(|| String::from("sin nombre"));
            line.push_str(&format!(
                " -- conectada: \"{}\" EDID {} B, {} modos, preferido {}x{}@{}.{:03} Hz ({} kHz){}{}",
                name,
                d.edid_size,
                d.num_modes,
                d.preferred_width,
                d.preferred_height,
                d.preferred_refresh_millihz / 1000,
                d.preferred_refresh_millihz % 1000,
                d.preferred_pixel_clock_khz,
                if d.vrr_supported != 0 { ", VRR" } else { "" },
                if d.is_dp_mst != 0 { ", DP-MST" } else { "" },
            ));
        }
        lines.push(line);
    }
    if info.num_displays as usize > MAX_DISPLAYS {
        lines.push(format!(
            "(y {} pantallas mas sin describir)",
            info.num_displays as usize - MAX_DISPLAYS
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edid_named(name: &[u8]) -> [u8; 128] {
        let mut e = [0u8; 128];
        e[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        // Second descriptor: the monitor name, newline-terminated.
        let d = &mut e[72..90];
        d[3] = 0xfc;
        d[5..5 + name.len()].copy_from_slice(name);
        d[5 + name.len()] = b'\n';
        e
    }

    #[test]
    fn the_monitor_name_comes_from_the_fc_descriptor() {
        assert_eq!(
            edid_monitor_name(&edid_named(b"DELL U2720Q")).as_deref(),
            Some("DELL U2720Q")
        );
        assert_eq!(edid_monitor_name(&[0u8; 128]), None, "no EDID header");
        let mut no_name = edid_named(b"X");
        no_name[72 + 3] = 0xff; // a serial number, not a name
        assert_eq!(edid_monitor_name(&no_name), None);
    }

    #[test]
    fn describe_names_each_display_and_says_which_are_connected() {
        let mut info = KmsInfo::zeroed();
        info.num_heads = 4;
        info.num_connectors = 4;
        info.num_displays = 2;
        info.max_width = 32767;
        info.max_height = 32767;
        info.max_cursor = 256;
        info.pitch_alignment = 256;
        info.vt_fb_base = 0xe000_0000;
        info.vt_fb_size = 0x7e_9000;
        info.displays[0].handle = 0x1;
        info.displays[0].connector_type = 0;
        info.displays[0].signal_format = 3;
        info.displays[0].head_mask = 0xf;
        info.displays[1].handle = 0x100;
        info.displays[1].connector_type = 6;
        info.displays[1].signal_format = 2;
        info.displays[1].physical_index = 3;
        info.displays[1].head_mask = 0xf;
        info.displays[1].connected = 1;
        info.displays[1].edid = edid_named(b"LG HDR 4K");
        info.displays[1].edid_size = 256;
        info.displays[1].num_modes = 31;
        info.displays[1].preferred_width = 3840;
        info.displays[1].preferred_height = 2160;
        info.displays[1].preferred_refresh_millihz = 60_000;
        info.displays[1].preferred_pixel_clock_khz = 594_000;

        let lines = describe(&info);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("4 cabezas, 4 conectores, 2 pantallas"));
        assert!(lines[0].contains("consola importada 0xe0000000+0x7e9000"));
        assert_eq!(
            lines[1],
            "pantalla 0x1: DP (DP) conector 0 cabezas 0xf -- desconectada"
        );
        assert_eq!(
            lines[2],
            "pantalla 0x100: HDMI (TMDS) conector 3 cabezas 0xf -- conectada: \"LG HDR 4K\" \
             EDID 256 B, 31 modos, preferido 3840x2160@60.000 Hz (594000 kHz)"
        );
    }

    #[test]
    fn describe_never_reads_past_the_described_displays() {
        let mut info = KmsInfo::zeroed();
        info.num_displays = 12;
        let lines = describe(&info);
        assert_eq!(lines.len(), 1 + MAX_DISPLAYS + 1);
        assert_eq!(lines.last().unwrap(), "(y 4 pantallas mas sin describir)");
        assert!(lines[0].contains("sin consola importada"));
    }

    #[test]
    fn a_hosted_build_allocates_no_device() {
        assert!(KmsDevice::alloc(0x100).is_none());
    }

    fn mode(width: u32, height: u32, hz: u32, preferred: bool) -> KmsMode {
        KmsMode {
            width,
            height,
            refresh_millihz: hz * 1000,
            pixel_clock_khz: 0,
            valid: 1,
            preferred: preferred as u8,
            interlaced: 0,
            pad: 0,
        }
    }

    #[test]
    fn the_console_mode_is_the_preferred_one_when_it_fits() {
        let modes = [
            mode(2560, 1440, 144, false),
            mode(1920, 1080, 144, false),
            mode(1920, 1080, 60, true),
        ];
        assert_eq!(console_mode(&modes, 1920, 1080), Some(2));
    }

    #[test]
    fn without_a_fitting_preferred_mode_the_console_runs_closest_to_60_hz() {
        let mut interlaced = mode(1920, 1080, 60, false);
        interlaced.interlaced = 1;
        let mut invalid = mode(1920, 1080, 60, false);
        invalid.valid = 0;
        let modes = [
            mode(3840, 2160, 60, true),
            interlaced,
            invalid,
            mode(1920, 1080, 120, false),
            mode(1920, 1080, 50, false),
            mode(1920, 1080, 75, false),
        ];
        assert_eq!(console_mode(&modes, 1920, 1080), Some(4));
        assert_eq!(console_mode(&modes, 1280, 720), None);
    }

    #[test]
    fn the_console_plan_gives_each_monitor_its_own_head() {
        let mut info = KmsInfo::zeroed();
        info.num_heads = 4;
        info.num_displays = 4;
        for (i, d) in info.displays.iter_mut().take(4).enumerate() {
            d.handle = 1 << i;
            d.head_mask = 0xf;
            d.connected = 1;
        }
        info.displays[1].connected = 0;
        // Display 0x4 can only use head 0, which display 0x1 already took.
        info.displays[2].head_mask = 0x1;
        let (sets, dark) = console_plan(
            &info,
            |handle| match handle {
                0x8 => alloc::vec![mode(1280, 1024, 60, true), mode(1920, 1080, 60, false)],
                _ => alloc::vec![mode(1920, 1080, 60, true)],
            },
            1920,
            1080,
        );
        assert_eq!(
            sets,
            [
                KmsHeadSet {
                    head: 0,
                    display: 0x1,
                    mode_index: 0
                },
                KmsHeadSet {
                    head: 1,
                    display: 0x8,
                    mode_index: 1
                },
            ]
        );
        assert_eq!(
            dark,
            ["pantalla 0x4: sin cabeza libre (cabezas 0x1); se queda apagada"]
        );
    }

    #[test]
    fn a_monitor_without_the_console_size_stays_dark() {
        let mut info = KmsInfo::zeroed();
        info.num_heads = 2;
        info.num_displays = 1;
        info.displays[0].handle = 0x100;
        info.displays[0].head_mask = 0x3;
        info.displays[0].connected = 1;
        let (sets, dark) = console_plan(
            &info,
            |_| alloc::vec![mode(1280, 1024, 60, true)],
            1920,
            1080,
        );
        assert!(sets.is_empty());
        assert_eq!(
            dark,
            ["pantalla 0x100: ningun modo de 1920x1080 para la consola; se queda apagada"]
        );
    }

    #[test]
    fn heads_past_the_device_are_never_planned() {
        let mut info = KmsInfo::zeroed();
        info.num_heads = 1;
        info.num_displays = 1;
        info.displays[0].handle = 0x1;
        info.displays[0].head_mask = 0x2;
        info.displays[0].connected = 1;
        let (sets, dark) = console_plan(
            &info,
            |_| alloc::vec![mode(1920, 1080, 60, true)],
            1920,
            1080,
        );
        assert!(sets.is_empty());
        assert_eq!(dark.len(), 1);
    }

    #[test]
    fn a_cursor_image_lands_top_left_in_a_transparent_square() {
        let img = cursor_image(&[0xff11_2233, 0x8000_0000, 0, 0x0102_0304], 2, 2).unwrap();
        assert_eq!(img.len(), 64 * 64 * 4);
        assert_eq!(&img[..8], &[0x33, 0x22, 0x11, 0xff, 0x00, 0x00, 0x00, 0x80]);
        let second_row = 64 * 4;
        assert_eq!(&img[second_row..second_row + 8], &[0, 0, 0, 0, 4, 3, 2, 1]);
        assert!(img[8..second_row].iter().all(|&b| b == 0));
        assert!(cursor_image(&[0; 65 * 65], 65, 65).is_none());
        assert!(cursor_image(&[0; 3], 2, 2).is_none(), "short image");
    }

    #[test]
    fn show_codes_match_the_c_side() {
        assert_eq!(ShowError::from_code(0), Ok(()));
        assert_eq!(ShowError::from_code(2), Err(ShowError::NoOwnership));
        assert_eq!(ShowError::from_code(3), Err(ShowError::ModeGone));
        assert_eq!(ShowError::from_code(4), Err(ShowError::Refused));
        assert_eq!(ShowError::from_code(5), Err(ShowError::CommitFailed));
        assert_eq!(ShowError::from_code(6), Err(ShowError::FlipPending));
        assert_eq!(ShowError::from_code(1), Err(ShowError::BadRequest));
    }
}
