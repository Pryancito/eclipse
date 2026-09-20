//! Nothing

/// Offset between a physical address and its kernel virtual mapping.
///
/// Always zero under libos: the "physical" addresses the allocator in
/// `zCore/src/memory.rs` hands out ARE host virtual addresses inside this
/// process, so the two address spaces coincide. `frame_alloc` subtracts this
/// offset from the pointer it returns and `frame_dealloc` adds it back, which
/// is why the two must agree rather than simply being unused.
///
/// The baremetal platforms (`x86`, `riscv`, `aarch64`) get theirs from the
/// bootloader. Without this one, `memory.rs` failed to resolve
/// `crate::platform::phys_to_virt_offset` (E0432) on every non-x86_64 libos
/// target — `zCore/src/main.rs` routes x86_64 to `memory_x86_64.rs`, so only
/// the aarch64 libos build reached this import and CI's
/// `Zircon Core Test Libos (aarch64 Linux)` job never compiled.
///
/// `#[allow(dead_code)]`: only `zCore/src/memory.rs` calls this, and
/// `zCore/src/main.rs` routes x86_64 to `memory_x86_64.rs` instead, so on an
/// x86_64 libos build nothing references it -- and `#![deny(warnings)]` turns
/// that into a hard error.
#[allow(dead_code)]
#[inline]
pub fn phys_to_virt_offset() -> usize {
    0
}
