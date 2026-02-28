//! GPU backend selection for `sidewind_wgpu`.
//!
//! `Backend` tells the `Instance` which underlying hardware path to use.
//! On Eclipse OS two real GPU families are supported:
//!
//! * **VirtIO** — `virtio-gpu` or `virtio-gpu-gl` (QEMU/KVM).  Uses the
//!   kernel's `virgl_*` and `gpu_*` syscalls.
//! * **NVIDIA** — Turing / Ampere / Ada / Hopper bare-metal.  Accesses the
//!   GPU through BAR0 MMIO via `sidewind_nvidia`.
//!
//! `Auto` probes both in order (VirtIO first, then NVIDIA) and selects
//! whichever is available.

/// Which GPU backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Probe VirtIO first, then NVIDIA; use whichever initialises successfully.
    Auto,
    /// VirtIO GPU (QEMU/KVM `virtio-gpu` or `virtio-gpu-gl`).
    VirtIo,
    /// Bare-metal NVIDIA GPU accessed via BAR0 MMIO.
    ///
    /// * `bar0_virt`  — virtual address of the 32 MB BAR0 mapping.
    /// * `vram_size_mb` — VRAM capacity in mebibytes (from `NV_PFB_CSTATUS`).
    Nvidia {
        bar0_virt: u64,
        vram_size_mb: u32,
    },
    /// Software fallback: write directly to the EFI GOP / VESA framebuffer.
    Framebuffer,
}

/// The concrete backend that was successfully initialised by `Instance::new`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveBackend {
    VirtIo,
    Nvidia,
    Framebuffer,
}
