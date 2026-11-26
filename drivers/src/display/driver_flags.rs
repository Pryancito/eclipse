//! The `nvidia.*` switches of the boot line as the kernel's NVIDIA driver
//! was told them, for the same driver in a process (`udrv-nvidia`), which
//! has no boot line of its own: it asks the kernel for them
//! (`ECLIPSE_DRIVER_FLAGS`) and is told them as the kernel's driver was,
//! by the same setters, before any GPU comes up. What the kernel decided
//! from the boot line (a switch another one implies, a default) travels,
//! not the line, so the two drivers cannot read it differently.
//!
//! On the wire: a little-endian `u64` of [`DriverFlags::bits`], then the
//! RM's registry (`nvidia.registry=`) as its `Key=Value;` text.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::convert::TryInto;

use super::nouveau_uapi;
use super::nvidia::{comptags, file_ctx, gem_vram, implicit_sync, nvkms_requested, pushbuf};

/// Each switch, and how it is read and set on this side.
const SWITCHES: [(u64, fn() -> bool, fn(bool)); 14] = [
    (
        NOUVEAU_UAPI,
        nouveau_uapi::requested,
        nouveau_uapi::set_requested,
    ),
    (
        1 << 1,
        nouveau_uapi::console_gsp_enabled,
        nouveau_uapi::set_console_gsp_enabled,
    ),
    (
        1 << 2,
        nouveau_uapi::gpu_trace_enabled,
        nouveau_uapi::set_gpu_trace_enabled,
    ),
    (
        1 << 3,
        nouveau_uapi::scanout_gpu_bringup,
        nouveau_uapi::set_scanout_gpu_bringup,
    ),
    (
        HWFLIP,
        nouveau_uapi::hwflip_enabled,
        nouveau_uapi::set_hwflip_enabled,
    ),
    (
        1 << 5,
        nouveau_uapi::surfaceflip_enabled,
        nouveau_uapi::set_surfaceflip_enabled,
    ),
    (
        1 << 6,
        nouveau_uapi::exec_fast_enabled,
        nouveau_uapi::set_exec_fast_enabled,
    ),
    (
        1 << 7,
        nouveau_uapi::compositor_ctx0,
        nouveau_uapi::set_compositor_ctx0,
    ),
    (1 << 8, file_ctx::enabled, file_ctx::set_enabled),
    (1 << 9, comptags::enabled, comptags::set_enabled),
    (1 << 10, pushbuf::enabled, pushbuf::set_enabled),
    (1 << 11, gem_vram::enabled, gem_vram::set_enabled),
    (1 << 12, nvkms_requested, super::set_nvkms_requested),
    (1 << 13, implicit_sync::enabled, implicit_sync::set_enabled),
];

/// `nvidia.nouveau_uapi`: asked for, which a process's card takes as on.
pub const NOUVEAU_UAPI: u64 = 1;

/// `nvidia.hwflip`: a flip is the GPU's copy engine copying the frame out
/// of the memory the kernel holds it in.
pub const HWFLIP: u64 = 1 << 4;

/// The longest registry text carried.
pub const REGISTRY_MAX: usize = 4096;

/// The most bytes on the wire.
pub const MAX: usize = 8 + REGISTRY_MAX;

/// The switches, as the kernel's driver was told them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DriverFlags {
    /// One bit per switch of [`SWITCHES`].
    pub bits: u64,
    /// `nvidia.registry=`, as the RM took it.
    pub registry: String,
}

impl DriverFlags {
    /// What this side's driver was told.
    pub fn this_boot() -> DriverFlags {
        let bits = SWITCHES
            .iter()
            .filter(|(_, get, _)| get())
            .fold(0, |bits, (bit, _, _)| bits | bit);
        let mut registry = String::new();
        for (key, value) in nvidia_rm_sys::registry::keys() {
            let entry = format!("{key}={value:#x};");
            if registry.len() + entry.len() > REGISTRY_MAX {
                break;
            }
            registry.push_str(&entry);
        }
        DriverFlags { bits, registry }
    }

    /// Tell this side's driver, as the kernel's was told at boot. A process
    /// that asks for the nouveau uAPI serves a card that has it, so it is
    /// on here as well as asked for.
    pub fn install(&self) {
        for (bit, _, set) in SWITCHES.iter() {
            set(self.bits & bit != 0);
        }
        if self.bits & NOUVEAU_UAPI != 0 {
            nouveau_uapi::set_enabled(true);
        }
        if !self.registry.is_empty() {
            super::set_nvidia_registry(&self.registry);
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.registry.len());
        out.extend_from_slice(&self.bits.to_le_bytes());
        out.extend_from_slice(self.registry.as_bytes());
        out
    }

    /// `None` for bytes that are not [`DriverFlags::encode`]'s.
    pub fn decode(bytes: &[u8]) -> Option<DriverFlags> {
        if bytes.len() < 8 || bytes.len() > MAX {
            return None;
        }
        let bits = u64::from_le_bytes(bytes[..8].try_into().ok()?);
        let registry = core::str::from_utf8(&bytes[8..]).ok()?.into();
        Some(DriverFlags { bits, registry })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switches_go_whole() {
        let flags = DriverFlags {
            bits: NOUVEAU_UAPI | 1 << 6 | 1 << 13,
            registry: "RMForcePcieConfigSave=0x1;".into(),
        };
        assert_eq!(DriverFlags::decode(&flags.encode()), Some(flags));
        assert_eq!(DriverFlags::decode(&[0; 7]), None);
        assert_eq!(DriverFlags::decode(&[0; MAX + 1]), None);
        let mut bad = DriverFlags::default().encode();
        bad.push(0xff);
        assert_eq!(DriverFlags::decode(&bad), None);
    }

    #[test]
    fn every_switch_has_a_bit_of_its_own() {
        let mut seen = 0u64;
        for (bit, _, _) in SWITCHES.iter() {
            assert_eq!(bit.count_ones(), 1);
            assert_eq!(seen & bit, 0);
            seen |= bit;
        }
    }

    /// The registry the kernel's driver took reads back the same in the
    /// process's.
    #[test]
    fn the_registry_reads_back_as_it_was_taken() {
        let keys = nvidia_rm_sys::registry::parse("A=1; B=0x20 ;C=010");
        let mut text = String::new();
        for (key, value) in &keys {
            text.push_str(&format!("{key}={value:#x};"));
        }
        assert_eq!(nvidia_rm_sys::registry::parse(&text), keys);
    }
}
