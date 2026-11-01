//! Pure lookup tables: what a number from the hardware or from RM means.
//!
//! Nothing here reads state or takes a lock. They are the tables Linux keeps
//! in its own headers and in `nvkms` -- connector types, the architecture
//! encoded in `PMC_BOOT_0`, the names of an MMU fault, the board behind a PCI
//! device id -- and a table in its own file is a table you can read next to
//! the vendor's without scrolling past six thousand lines of driver.

use super::*;

/// NV0073_CTRL_SPECIFIC_CONNECTOR_DATA_TYPE_* -> DRM_MODE_CONNECTOR_*.
pub(super) fn nv_conn_type_to_drm(t: u32) -> u32 {
    match t {
        0x00 => 1,                              // VGA_15_PIN
        0x30 | 0x38 | 0x39 => 2,                // DVI_I / LFH_DVI_I_{1,2}
        0x31 => 3,                              // DVI_D
        0x46 | 0x47 | 0x49 | 0x64 | 0x65 => 10, // DP ext/int/serializer, LFH_DP
        0x48 => 10,                             // DP_MINI_EXT
        0x61 | 0x63 => 11,                      // HDMI_A / HDMI_C_MINI
        0x70 => 15,                             // VIRTUAL_WFD
        0x71 | 0x74 => 10,                      // USB_C (DP alt mode)
        0x72 => 16,                             // DSI
        _ => 0,                                 // Unknown
    }
}

/// Short human name for the /proc/gpuedid dump.
pub(super) fn nv_conn_type_name(t: u32) -> &'static str {
    match t {
        0x00 => "VGA",
        0x30 | 0x38 | 0x39 => "DVI-I",
        0x31 => "DVI-D",
        0x46 | 0x47 | 0x49 | 0x64 | 0x65 => "DP",
        0x48 => "miniDP",
        0x61 => "HDMI",
        0x63 => "miniHDMI",
        0x70 => "virtual",
        0x71 | 0x74 => "USB-C",
        0x72 => "DSI",
        0xFFFF_FFFF => "?",
        _ => "other",
    }
}

#[allow(dead_code)] // used when deferred BAR0 MMIO probe is enabled
pub(super) fn arch_from_pmc_boot0(boot0: u32) -> NvidiaArchitecture {
    let chip_id = (boot0 >> regs::PMC_BOOT0_CHIP_ID_SHIFT) & regs::PMC_BOOT0_CHIP_ID_MASK;
    if chip_id >= regs::PMC_BOOT0_CHIPID_BLACKWELL_MIN {
        NvidiaArchitecture::Blackwell
    } else if (regs::PMC_BOOT0_CHIPID_HOPPER_MIN..=regs::PMC_BOOT0_CHIPID_HOPPER_MAX)
        .contains(&chip_id)
    {
        NvidiaArchitecture::Hopper
    } else if (regs::PMC_BOOT0_CHIPID_ADA_MIN..=regs::PMC_BOOT0_CHIPID_ADA_MAX).contains(&chip_id) {
        NvidiaArchitecture::AdaLovelace
    } else if (regs::PMC_BOOT0_CHIPID_AMPERE_MIN..=regs::PMC_BOOT0_CHIPID_AMPERE_MAX)
        .contains(&chip_id)
    {
        NvidiaArchitecture::Ampere
    } else if (regs::PMC_BOOT0_CHIPID_TURING_MIN..=regs::PMC_BOOT0_CHIPID_TURING_MAX)
        .contains(&chip_id)
    {
        NvidiaArchitecture::Turing
    } else {
        NvidiaArchitecture::Unknown
    }
}

/// NV_PFAULT_FAULT_TYPE ([4:0] of INFO1) decode (Turing dev_fault.ref.txt).
pub(super) fn fault_reason_name(r: u32) -> &'static str {
    match r {
        0 => "PDE",
        1 => "PDE_SIZE",
        2 => "PTE(unmapped)",
        3 => "VA_LIMIT",
        4 => "UNBOUND_INST",
        5 => "PRIV",
        6 => "RO",
        7 => "WO",
        0xa => "BAD_APERTURE",
        _ => "?",
    }
}

/// NV_PFAULT_ACCESS_TYPE ([19:16] of INFO1) decode.
pub(super) fn fault_access_name(a: u32) -> &'static str {
    match a {
        0 => "READ",
        1 => "WRITE",
        2 => "ATOMIC",
        3 => "PREFETCH",
        8 => "PHYS_READ",
        9 => "PHYS_WRITE",
        0xa => "PHYS_ATOMIC",
        _ => "?",
    }
}

pub(super) fn read_temperature(bar0: usize) -> Option<i32> {
    let raw =
        unsafe { core::ptr::read_volatile((bar0 + regs::NV_THERM_TEMP as usize) as *const u32) };
    if raw == 0 || raw == 0xFFFF_FFFF {
        return None;
    }
    let raw9 = raw & regs::NV_THERM_TEMP_VALUE_MASK;
    if (raw9 & regs::NV_THERM_TEMP_VALUE_SIGN_BIT) != 0 {
        Some((raw9 as i32) - 512)
    } else {
        Some(raw9 as i32)
    }
}

#[allow(dead_code)]
unsafe fn probe_resolution_from_bar0(bar0: usize) -> Option<(u32, u32)> {
    let reg =
        core::ptr::read_volatile((bar0 + regs::NV50_HEAD0_RASTER_SIZE as usize) as *const u32);
    let (w, h) = (reg & 0xFFFF, reg >> 16);
    if w > 0 && h > 0 && w <= 16384 && h <= 16384 {
        return Some((w, h));
    }

    let reg = core::ptr::read_volatile((bar0 + regs::NV40_PCRTC_HEAD0_SIZE as usize) as *const u32);
    let (w, h) = (reg & 0xFFFF, reg >> 16);
    if w > 0 && h > 0 && w <= 16384 && h <= 16384 {
        return Some((w, h));
    }
    None
}

/// Identify GPU based on PCI device ID.
/// Returns (architecture, name, memory_mb).
pub(super) fn identify_gpu(device_id: u16) -> (NvidiaArchitecture, &'static str, u32) {
    match device_id {
        // Blackwell
        0x2B85 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5090", 32768),
        0x2B89 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5080", 16384),
        0x2C00 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5070 Ti", 16384),
        0x2C20 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5070", 12288),

        // Ada Lovelace
        0x2684 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4090", 24576),
        0x2704 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4080", 16384),
        0x2782 => (
            NvidiaArchitecture::AdaLovelace,
            "GeForce RTX 4070 Ti",
            12288,
        ),
        0x2786 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070", 12288),
        0x2803 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060 Ti", 8192),
        0x2882 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060", 8192),

        // Ampere
        0x2204 => (NvidiaArchitecture::Ampere, "GeForce RTX 3090", 24576),
        0x2206 => (NvidiaArchitecture::Ampere, "GeForce RTX 3080", 10240),
        0x2484 => (NvidiaArchitecture::Ampere, "GeForce RTX 3070", 8192),
        0x2489 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060 Ti", 8192),
        0x2503 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060", 12288),
        0x2571 => (NvidiaArchitecture::Ampere, "GeForce RTX 3050", 8192),

        // Turing
        0x1E02 => (NvidiaArchitecture::Turing, "GeForce RTX 2080 Ti", 11264),
        0x1E04 => (NvidiaArchitecture::Turing, "GeForce RTX 2080 Super", 8192),
        0x1E07 => (NvidiaArchitecture::Turing, "GeForce RTX 2080", 8192),
        0x1E82 => (NvidiaArchitecture::Turing, "GeForce RTX 2070 Super", 8192),
        0x1E84 => (NvidiaArchitecture::Turing, "GeForce RTX 2070", 8192),
        0x1F02 | 0x1F06 | 0x1F07 => (NvidiaArchitecture::Turing, "GeForce RTX 2060 Super", 8192),
        0x1F03 | 0x1F08 | 0x1F0A | 0x1F0B => (NvidiaArchitecture::Turing, "GeForce RTX 2060", 6144),
        0x1F36 => (NvidiaArchitecture::Turing, "GeForce GTX 1660 Super", 6144),
        0x1F82 => (NvidiaArchitecture::Turing, "GeForce GTX 1660", 6144),
        0x1F91 => (NvidiaArchitecture::Turing, "GeForce GTX 1650 Super", 4096),
        0x1F99 => (NvidiaArchitecture::Turing, "GeForce GTX 1650", 4096),

        _ => (NvidiaArchitecture::Unknown, "Unknown NVIDIA GPU", 0),
    }
}
