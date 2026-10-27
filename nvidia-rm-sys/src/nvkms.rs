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

    /// kapi's `freeDevice`.
    pub fn free(self) {
        #[cfg(target_os = "none")]
        unsafe {
            eclipse_kms_free_device(self.0.as_ptr())
        };
    }
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
}
