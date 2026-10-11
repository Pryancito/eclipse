//! Register offsets and constants (aligned with Nova / open-gpu-kernel-modules).

pub const NV_PMC_BOOT_0: u32 = 0x0000_0000;
pub const PMC_BOOT0_CHIP_ID_SHIFT: u32 = 20;
pub const PMC_BOOT0_CHIP_ID_MASK: u32 = 0xFFF;

pub const PMC_BOOT0_CHIPID_TURING_MIN: u32 = 0x160;
pub const PMC_BOOT0_CHIPID_TURING_MAX: u32 = 0x16F;
pub const PMC_BOOT0_CHIPID_AMPERE_MIN: u32 = 0x170;
pub const PMC_BOOT0_CHIPID_AMPERE_MAX: u32 = 0x17F;
pub const PMC_BOOT0_CHIPID_ADA_MIN: u32 = 0x190;
pub const PMC_BOOT0_CHIPID_ADA_MAX: u32 = 0x19F;
// The chip-id field is NINE bits wide (nouveau reads it as
// `(boot0 & 0x1ff00000) >> 20`, which `nouveau_chipset_id` below does
// too), so no real part can ever report an id of 0x200 or above. Hopper
// used to sit at 0x1B0..=0x1BF and Blackwell at `>= 0x200`, which made the
// Blackwell arm unreachable and handed every consumer Blackwell id to
// Hopper. The two bounds here are the ones this file's own
// `nouveau_chipset_id` fallback table already names as representative:
// 0x180 = GH100 (Hopper) and 0x1b2 = GB202 (Blackwell).
pub const PMC_BOOT0_CHIPID_HOPPER_MIN: u32 = 0x180;
pub const PMC_BOOT0_CHIPID_HOPPER_MAX: u32 = 0x18F;
pub const PMC_BOOT0_CHIPID_BLACKWELL_MIN: u32 = 0x1A0;

pub const NV_PFB_CSTATUS: u32 = 0x0010_020C;
pub const NV_PFB_CSTATUS_MEM_SIZE_MASK: u32 = 0x7FFF;

pub const NV_THERM_TEMP: u32 = 0x0002_0400;
pub const NV_THERM_TEMP_VALUE_MASK: u32 = 0x1FF;
pub const NV_THERM_TEMP_VALUE_SIGN_BIT: u32 = 0x100;

// Display resolution registers (legacy/fallback)
pub const NV50_HEAD0_RASTER_SIZE: u32 = 0x610798;
pub const NV40_PCRTC_HEAD0_SIZE: u32 = 0x60002C;
