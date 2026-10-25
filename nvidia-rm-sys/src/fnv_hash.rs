//! FFI bindings for the first real (non-hand-written) vendored NVIDIA
//! source: src/nvidia/src/libraries/fnv_hash/fnv_hash.c. Only linkable
//! once the submodule is checked out and build.rs actually compiles it
//! (see build.rs's build_first_real_nvidia_file) -- calling this without
//! the submodule present fails at kernel link time with an unresolved
//! symbol, same as any other hard dependency on vendored code.
use crate::types::{NvU32, NvU64, NvU8};

#[cfg(target_os = "none")]
extern "C" {
    fn fnv1Hash64(data: *const NvU8, data_len: NvU32) -> NvU64;
}

/// Hosted (libos) builds have no NVIDIA C to link (`build.rs` compiles it
/// for `target_os = "none"` only), and `/proc/gpudbg` hashes two fixed
/// strings through this on every read: the same FNV-1 (multiply, then
/// xor; not FNV-1a) the vendored file computes, so the dump's "expect"
/// line holds on a host test too.
#[cfg(not(target_os = "none"))]
#[allow(non_snake_case)]
unsafe fn fnv1Hash64(data: *const NvU8, data_len: NvU32) -> NvU64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let bytes = core::slice::from_raw_parts(data, data_len as usize);
    bytes
        .iter()
        .fold(OFFSET_BASIS, |h, b| h.wrapping_mul(PRIME) ^ u64::from(*b))
}

/// Safe wrapper around NVIDIA's real `fnv1Hash64`. Used as the first
/// real-hardware proof that actual vendored NVIDIA C (not the hand-written
/// smoke test) runs correctly inside Eclipse's kernel.
pub fn fnv1_hash64(data: &[u8]) -> u64 {
    unsafe { fnv1Hash64(data.as_ptr(), data.len() as NvU32) }
}
