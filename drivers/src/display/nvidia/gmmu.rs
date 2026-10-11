//! TU106 (Turing) GMMU encode helpers: see the `mod gmmu;` doc in `nvidia.rs`.

/// SYSTEM_COHERENT aperture (HOST). VRAM=0, HOST=2, NCOH=3.
pub const AP_HOST: u64 = 2;
/// PITCH (uncompressed) kind.
pub const KIND_PITCH: u64 = 0x00;

/// Leaf PTE for a 4 KiB sysmem page, read-write, uncompressed.
/// VALID(0) | APERTURE 2:1 = HOST | VOL(3) | ADDRESS=phys>>4 | KIND 63:56.
#[inline]
pub fn encode_pte_sys(phys: u64) -> u64 {
    (phys >> 4) | (1 << 0) | (AP_HOST << 1) | (1 << 3) | (KIND_PITCH << 56)
}

/// Single PDE (PD1/PD2/PD3 levels) pointing at the next table in sysmem.
/// APERTURE 2:1 = HOST (aperture != 0 ⇒ present; there is no VALID bit) |
/// VOL(3) | ADDRESS_SYS 53:8 = next>>4. The dual-PDE SMALL half is encoded
/// identically and stored in the high qword at byte `pdei*0x10 + 8`.
#[inline]
pub fn encode_pde_sys(next_table_phys: u64) -> u64 {
    (next_table_phys >> 4) | (AP_HOST << 1) | (1 << 3)
}

/// Instance-block PD-base qword (@0x200): root PD phys OR'd with
/// VER2(1<<10) | 64KiB(1<<11) | HOST_target(2<<0) | VOL(1<<2) == `|0xC06`.
#[inline]
pub fn inst_pd_base(root_phys: u64) -> u64 {
    root_phys | 0xC06
}
