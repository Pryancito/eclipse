//! The x86-64 halves of `zx_thread_read_state`/`zx_thread_write_state` that
//! are not the general registers: the x87 state, the vector state and the
//! single-step flag.
//!
//! Each is a pure function over the thread's extended-state save area (the
//! bytes XSAVE or FXSAVE wrote, see `UserContext::extended_state`) and the
//! caller's buffer, so every path here runs on a host `cargo test` with a
//! save area built by hand. The layouts are the CPU's (Intel SDM vol. 1,
//! "XSAVE-managed state") and Zircon's `zx_thread_state_*_t` in
//! `zircon/syscalls/debug.h`; the offsets are checked where they are used.
//!
//! What Zircon's own `arch/x86/debugger.cc` hands a debugger is what is
//! handed here: the low 128 bits of `xmm0`-`xmm15`, their AVX high halves
//! when the CPU has them, and `mxcsr`; the AVX-512 parts of the vector
//! struct stay zero. The core tests (`vector_regs_expect_unsupported_are_zero`)
//! count on that.

use crate::{ZxError, ZxResult};
use core::convert::TryInto;

/// The bits of `rflags` a thread may set on itself through `write_state`:
/// CF, PF, AF, ZF, SF, TF, DF, OF, NT, AC and ID. The rest -- IF above all --
/// are the kernel's, and a write keeps whatever they were.
pub(super) const X86_FLAGS_USER: usize = 0x24_4dd5;

/// The trap flag, which is all `ZX_THREAD_STATE_SINGLE_STEP` is.
const X86_FLAGS_TF: usize = 1 << 8;

/// The MXCSR bits every SSE CPU has, for when the save area's own
/// `mxcsr_mask` is zero (an FXSAVE that never ran, or a CPU that reports
/// none): what `debugger.cc` falls back to.
const MXCSR_DEFAULT_MASK: u32 = 0x0000_ffbf;

/// XSTATE_BV bits of the components this module reads and writes.
const XSTATE_X87: u64 = 1 << 0;
const XSTATE_SSE: u64 = 1 << 1;
const XSTATE_AVX: u64 = 1 << 2;

// Offsets in the save area, standard (non-compacted) format.
const FCW: usize = 0;
const FSW: usize = 2;
const FTW: usize = 4;
const FOP: usize = 6;
const FIP: usize = 8;
const FDP: usize = 16;
const MXCSR: usize = 24;
const MXCSR_MASK: usize = 28;
const ST: usize = 32;
const XMM: usize = 160;
const XSTATE_BV: usize = 512;
const YMM_HI: usize = 576;
const LEGACY_AND_HEADER: usize = 576;
const AVX_END: usize = YMM_HI + 16 * 16;

/// `zx_thread_state_fp_regs_t`: 160 bytes.
pub(super) const FP_REGS_SIZE: usize = 160;
/// `zx_thread_state_vector_regs_t`: 32 zmm of 64 bytes, 8 opmask words,
/// mxcsr and 4 bytes of padding.
pub(super) const VECTOR_REGS_SIZE: usize = 32 * 64 + 8 * 8 + 4 + 4;
const VECTOR_OPMASK: usize = 32 * 64;
const VECTOR_MXCSR: usize = VECTOR_OPMASK + 8 * 8;
/// `zx_thread_state_single_step_t`: a `uint32_t`.
pub(super) const SINGLE_STEP_SIZE: usize = 4;

/// Whether component `bit` of the save area holds the thread's registers.
///
/// On the XSAVE path (`components != 0`) a clear `XSTATE_BV` bit means the
/// component is in its initial state and the bytes may be stale; on the
/// FXSAVE path the header is never written and the legacy region is always
/// current.
fn present(area: &[u8], components: u32, bit: u64) -> bool {
    if components == 0 {
        return bit == XSTATE_X87 || bit == XSTATE_SSE;
    }
    if u64::from(components) & bit == 0 {
        return false;
    }
    xstate_bv(area) & bit != 0
}

fn xstate_bv(area: &[u8]) -> u64 {
    u64::from_le_bytes(area[XSTATE_BV..XSTATE_BV + 8].try_into().unwrap())
}

/// Marks component `bit` as holding registers, so the next XRSTOR loads
/// what was written rather than the initial state. Only for components the
/// CPU enabled: XRSTOR faults on any other bit.
fn mark_present(area: &mut [u8], components: u32, bit: u64) {
    if components == 0 || u64::from(components) & bit == 0 {
        return;
    }
    let bv = xstate_bv(area) | bit;
    area[XSTATE_BV..XSTATE_BV + 8].copy_from_slice(&bv.to_le_bytes());
}

/// `ZX_THREAD_STATE_FP_REGS` out of the save area. Reserved fields come out
/// zero, and a thread that never touched the x87 reads as the initial state.
pub(super) fn read_fp_regs(area: &[u8], components: u32, out: &mut [u8]) -> ZxResult<usize> {
    let out = out
        .get_mut(..FP_REGS_SIZE)
        .ok_or(ZxError::BUFFER_TOO_SMALL)?;
    out.fill(0);
    if !present(area, components, XSTATE_X87) {
        // The initial x87 state: control word 0x37f, every register tag
        // empty, nothing else.
        out[0..2].copy_from_slice(&0x037fu16.to_le_bytes());
        return Ok(FP_REGS_SIZE);
    }
    out[0..2].copy_from_slice(&area[FCW..FCW + 2]);
    out[2..4].copy_from_slice(&area[FSW..FSW + 2]);
    out[4] = area[FTW];
    out[6..8].copy_from_slice(&area[FOP..FOP + 2]);
    out[8..16].copy_from_slice(&area[FIP..FIP + 8]);
    out[16..24].copy_from_slice(&area[FDP..FDP + 8]);
    out[32..160].copy_from_slice(&area[ST..ST + 128]);
    Ok(FP_REGS_SIZE)
}

/// `ZX_THREAD_STATE_FP_REGS` into the save area.
pub(super) fn write_fp_regs(area: &mut [u8], components: u32, buf: &[u8]) -> ZxResult {
    let buf = buf.get(..FP_REGS_SIZE).ok_or(ZxError::BUFFER_TOO_SMALL)?;
    area[FCW..FCW + 2].copy_from_slice(&buf[0..2]);
    area[FSW..FSW + 2].copy_from_slice(&buf[2..4]);
    area[FTW] = buf[4];
    area[FOP..FOP + 2].copy_from_slice(&buf[6..8]);
    area[FIP..FIP + 8].copy_from_slice(&buf[8..16]);
    area[FDP..FDP + 8].copy_from_slice(&buf[16..24]);
    area[ST..ST + 128].copy_from_slice(&buf[32..160]);
    mark_present(area, components, XSTATE_X87);
    Ok(())
}

/// `ZX_THREAD_STATE_VECTOR_REGS` out of the save area: `xmm0`-`xmm15` in
/// `zmm[i].v[0..2]`, the AVX high halves in `v[2..4]` when the CPU saves
/// them, `mxcsr`; everything else zero.
pub(super) fn read_vector_regs(area: &[u8], components: u32, out: &mut [u8]) -> ZxResult<usize> {
    let out = out
        .get_mut(..VECTOR_REGS_SIZE)
        .ok_or(ZxError::BUFFER_TOO_SMALL)?;
    out.fill(0);
    let mxcsr = if present(area, components, XSTATE_SSE) {
        for i in 0..16 {
            out[i * 64..i * 64 + 16].copy_from_slice(&area[XMM + i * 16..XMM + i * 16 + 16]);
        }
        area[MXCSR..MXCSR + 4].try_into().unwrap()
    } else {
        0x1f80u32.to_le_bytes()
    };
    out[VECTOR_MXCSR..VECTOR_MXCSR + 4].copy_from_slice(&mxcsr);
    if present(area, components, XSTATE_AVX) {
        for i in 0..16 {
            out[i * 64 + 16..i * 64 + 32]
                .copy_from_slice(&area[YMM_HI + i * 16..YMM_HI + i * 16 + 16]);
        }
    }
    Ok(VECTOR_REGS_SIZE)
}

/// `ZX_THREAD_STATE_VECTOR_REGS` into the save area. An `mxcsr` with a bit
/// the CPU does not have is `INVALID_ARGS` before anything is written: loaded,
/// it would be a #GP on the way back to the thread (fxbug.dev/42127734).
pub(super) fn write_vector_regs(area: &mut [u8], components: u32, buf: &[u8]) -> ZxResult {
    let buf = buf
        .get(..VECTOR_REGS_SIZE)
        .ok_or(ZxError::BUFFER_TOO_SMALL)?;
    let mxcsr = u32::from_le_bytes(buf[VECTOR_MXCSR..VECTOR_MXCSR + 4].try_into().unwrap());
    let mut mask = u32::from_le_bytes(area[MXCSR_MASK..MXCSR_MASK + 4].try_into().unwrap());
    if mask == 0 {
        mask = MXCSR_DEFAULT_MASK;
    }
    if mxcsr & !mask != 0 {
        return Err(ZxError::INVALID_ARGS);
    }
    for i in 0..16 {
        area[XMM + i * 16..XMM + i * 16 + 16].copy_from_slice(&buf[i * 64..i * 64 + 16]);
    }
    area[MXCSR..MXCSR + 4].copy_from_slice(&mxcsr.to_le_bytes());
    mark_present(area, components, XSTATE_SSE);
    if components != 0 && u64::from(components) & XSTATE_AVX != 0 {
        for i in 0..16 {
            area[YMM_HI + i * 16..YMM_HI + i * 16 + 16]
                .copy_from_slice(&buf[i * 64 + 16..i * 64 + 32]);
        }
        mark_present(area, components, XSTATE_AVX);
    }
    Ok(())
}

/// `ZX_THREAD_STATE_SINGLE_STEP`: 1 when the trap flag is set.
pub(super) fn read_single_step(rflags: usize, out: &mut [u8]) -> ZxResult<usize> {
    let out = out
        .get_mut(..SINGLE_STEP_SIZE)
        .ok_or(ZxError::BUFFER_TOO_SMALL)?;
    let value: u32 = u32::from(rflags & X86_FLAGS_TF != 0);
    out.copy_from_slice(&value.to_le_bytes());
    Ok(SINGLE_STEP_SIZE)
}

/// `ZX_THREAD_STATE_SINGLE_STEP` into `rflags`: 0 and 1 are the only values.
pub(super) fn write_single_step(rflags: &mut usize, buf: &[u8]) -> ZxResult {
    let buf = buf
        .get(..SINGLE_STEP_SIZE)
        .ok_or(ZxError::BUFFER_TOO_SMALL)?;
    match u32::from_le_bytes(buf.try_into().unwrap()) {
        0 => *rflags &= !X86_FLAGS_TF,
        1 => *rflags |= X86_FLAGS_TF,
        _ => return Err(ZxError::INVALID_ARGS),
    }
    Ok(())
}

/// Whether a general-register write may be applied: the instruction pointer
/// must lie below `user_end`, the limit of the thread's own address space
/// (`VmAddressRegion::user_limit`: the power of two at or past its root
/// VMAR's end), and the two thread-pointer bases must be canonical.
///
/// A non-canonical `rip` would fault on the `iretq`/`sysret` back to the
/// thread -- in the kernel, on the kernel's stack
/// (`docs/concepts/kernel/sysret_problem.md`) -- and no thread has business
/// returning to a kernel address. Zircon draws that line at the canonical
/// user half; the core tests draw it at `bit_ceil` of the root VMAR's end
/// (`X86LowestNonCanonicalAddr`), expect the address just below it to be
/// accepted, and on this kernel that is lower than the canonical half: a
/// hosted process gets a 64 GiB window of the host's address space.
///
/// The bases get Zircon's own rule, canonical, and not the window's end: a
/// hosted thread that never set a TLS of its own reads back the FS base the
/// kernel lent it, which is an address of the HOST's (`POP_USER_FSBASE`
/// hands out `kernel_fsbase + 72`), and a debugger writing a thread's
/// registers back unchanged must not be refused.
pub(super) fn addresses_are_writable(
    rip: usize,
    fs_base: usize,
    gs_base: usize,
    user_end: usize,
) -> ZxResult {
    if rip >= user_end || !is_canonical(fs_base) || !is_canonical(gs_base) {
        return Err(ZxError::INVALID_ARGS);
    }
    Ok(())
}

/// A 48-bit canonical address: the top 17 bits all equal.
fn is_canonical(addr: usize) -> bool {
    let top = addr >> 47;
    top == 0 || top == (1 << 17) - 1
}

/// The `rflags` a thread ends up with after writing `requested` over
/// `saved`: its own bits from the request, the kernel's kept.
pub(super) fn user_rflags(saved: usize, requested: usize) -> usize {
    (saved & !X86_FLAGS_USER) | (requested & X86_FLAGS_USER)
}

const _: () = {
    assert!(AVX_END <= 2688, "the AVX high halves fit the save area");
    assert!(LEGACY_AND_HEADER == 512 + 64);
    assert!(VECTOR_REGS_SIZE == 2120, "zx_thread_state_vector_regs_t");
};

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: u32 = 0b111;

    fn area() -> [u8; 2688] {
        let mut a = [0u8; 2688];
        a[FCW..FCW + 2].copy_from_slice(&0x037fu16.to_le_bytes());
        a[MXCSR..MXCSR + 4].copy_from_slice(&0x1f80u32.to_le_bytes());
        a[MXCSR_MASK..MXCSR_MASK + 4].copy_from_slice(&0xffffu32.to_le_bytes());
        a
    }

    fn set_bv(a: &mut [u8], bv: u64) {
        a[XSTATE_BV..XSTATE_BV + 8].copy_from_slice(&bv.to_le_bytes());
    }

    #[test]
    fn the_x87_registers_come_out_of_the_legacy_area() {
        let mut a = area();
        for i in 0..8 {
            a[ST + i * 16..ST + i * 16 + 8].copy_from_slice(&(i as u64).to_le_bytes());
        }
        a[FTW] = 0xff;
        a[FOP..FOP + 2].copy_from_slice(&0x1234u16.to_le_bytes());
        set_bv(&mut a, XSTATE_X87);
        let mut out = [0xaau8; 200];
        assert_eq!(read_fp_regs(&a, ALL, &mut out), Ok(FP_REGS_SIZE));
        assert_eq!(u16::from_le_bytes([out[0], out[1]]), 0x037f);
        assert_eq!(out[4], 0xff);
        assert_eq!(u16::from_le_bytes([out[6], out[7]]), 0x1234);
        for i in 0..8 {
            let lo = u64::from_le_bytes(out[32 + i * 16..32 + i * 16 + 8].try_into().unwrap());
            assert_eq!(lo, i as u64, "st{}", i);
        }
        // Reserved bytes are not leaked; the buffer past the struct is left.
        assert_eq!(out[5], 0);
        assert_eq!(&out[24..32], &[0; 8]);
        assert_eq!(out[160], 0xaa);
    }

    #[test]
    fn an_x87_never_touched_reads_as_the_initial_state() {
        let mut a = area();
        a[ST..ST + 8].copy_from_slice(&[0xee; 8]); // stale bytes
        a[FTW] = 0x11;
        set_bv(&mut a, XSTATE_SSE); // x87 not in use
        let mut out = [0u8; FP_REGS_SIZE];
        read_fp_regs(&a, ALL, &mut out).unwrap();
        assert_eq!(u16::from_le_bytes([out[0], out[1]]), 0x037f);
        assert_eq!(out[4], 0);
        assert_eq!(&out[32..40], &[0; 8]);
        // On the FXSAVE path the legacy area is always what the thread has.
        let mut out = [0u8; FP_REGS_SIZE];
        read_fp_regs(&a, 0, &mut out).unwrap();
        assert_eq!(&out[32..40], &[0xee; 8]);
        assert_eq!(out[4], 0x11);
    }

    #[test]
    fn writing_the_x87_registers_marks_them_in_use() {
        let mut a = area();
        let mut buf = [0u8; FP_REGS_SIZE];
        buf[32..40].copy_from_slice(&7u64.to_le_bytes());
        buf[4] = 0x80;
        write_fp_regs(&mut a, ALL, &buf).unwrap();
        assert_eq!(&a[ST..ST + 8], &7u64.to_le_bytes());
        assert_eq!(a[FTW], 0x80);
        assert_eq!(xstate_bv(&a), XSTATE_X87);
        // And reads back.
        let mut out = [0u8; FP_REGS_SIZE];
        read_fp_regs(&a, ALL, &mut out).unwrap();
        assert_eq!(&out[32..40], &7u64.to_le_bytes());
        // The header is not touched on the FXSAVE path.
        let mut a = area();
        write_fp_regs(&mut a, 0, &buf).unwrap();
        assert_eq!(xstate_bv(&a), 0);
    }

    #[test]
    fn the_vector_registers_are_xmm_their_avx_halves_and_mxcsr() {
        let mut a = area();
        for i in 0..16 {
            a[XMM + i * 16..XMM + i * 16 + 8].copy_from_slice(&(i as u64).to_le_bytes());
            a[XMM + i * 16 + 8..XMM + i * 16 + 16]
                .copy_from_slice(&((i as u64) << 8).to_le_bytes());
            a[YMM_HI + i * 16..YMM_HI + i * 16 + 8]
                .copy_from_slice(&((i as u64) << 16).to_le_bytes());
        }
        a[MXCSR..MXCSR + 4].copy_from_slice(&0x1f00u32.to_le_bytes());
        set_bv(&mut a, XSTATE_SSE | XSTATE_AVX);
        let mut out = [0xaau8; VECTOR_REGS_SIZE + 8];
        assert_eq!(read_vector_regs(&a, ALL, &mut out), Ok(VECTOR_REGS_SIZE));
        let word = |i: usize, v: usize| {
            u64::from_le_bytes(out[i * 64 + v * 8..i * 64 + v * 8 + 8].try_into().unwrap())
        };
        for i in 0..16 {
            assert_eq!(word(i, 0), i as u64);
            assert_eq!(word(i, 1), (i as u64) << 8);
            assert_eq!(word(i, 2), (i as u64) << 16);
            assert_eq!(word(i, 3), 0);
            // The AVX-512 parts are not handed out: zero.
            for v in 4..8 {
                assert_eq!(word(i, v), 0);
            }
            for v in 0..8 {
                assert_eq!(word(16 + i, v), 0);
            }
        }
        for k in 0..8 {
            assert_eq!(
                &out[VECTOR_OPMASK + k * 8..VECTOR_OPMASK + k * 8 + 8],
                &[0; 8]
            );
        }
        assert_eq!(
            u32::from_le_bytes(out[VECTOR_MXCSR..VECTOR_MXCSR + 4].try_into().unwrap()),
            0x1f00
        );
        assert_eq!(&out[VECTOR_MXCSR + 4..VECTOR_REGS_SIZE], &[0; 4]);
        assert_eq!(out[VECTOR_REGS_SIZE], 0xaa);
    }

    #[test]
    fn avx_halves_the_cpu_does_not_save_read_as_zero() {
        let mut a = area();
        a[YMM_HI..YMM_HI + 8].copy_from_slice(&[0xee; 8]);
        set_bv(&mut a, XSTATE_SSE | XSTATE_AVX);
        // A CPU without AVX in XCR0.
        let mut out = [0u8; VECTOR_REGS_SIZE];
        read_vector_regs(&a, 0b11, &mut out).unwrap();
        assert_eq!(&out[16..24], &[0; 8]);
        // An AVX CPU whose thread has AVX in the initial state.
        set_bv(&mut a, XSTATE_SSE);
        read_vector_regs(&a, ALL, &mut out).unwrap();
        assert_eq!(&out[16..24], &[0; 8]);
        // SSE in the initial state: xmm zero, mxcsr the default.
        a[XMM..XMM + 8].copy_from_slice(&[0xee; 8]);
        a[MXCSR..MXCSR + 4].copy_from_slice(&0u32.to_le_bytes());
        set_bv(&mut a, 0);
        read_vector_regs(&a, ALL, &mut out).unwrap();
        assert_eq!(&out[0..8], &[0; 8]);
        assert_eq!(
            u32::from_le_bytes(out[VECTOR_MXCSR..VECTOR_MXCSR + 4].try_into().unwrap()),
            0x1f80
        );
    }

    #[test]
    fn writing_the_vector_registers_lands_in_the_area_and_marks_them() {
        let mut a = area();
        let mut buf = [0u8; VECTOR_REGS_SIZE];
        for i in 0..16 {
            buf[i * 64..i * 64 + 8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
            buf[i * 64 + 16..i * 64 + 24].copy_from_slice(&(i as u64 + 100).to_le_bytes());
        }
        buf[VECTOR_MXCSR..VECTOR_MXCSR + 4].copy_from_slice(&0x1f81u32.to_le_bytes());
        write_vector_regs(&mut a, ALL, &buf).unwrap();
        assert_eq!(&a[XMM + 15 * 16..XMM + 15 * 16 + 8], &16u64.to_le_bytes());
        assert_eq!(
            &a[YMM_HI + 15 * 16..YMM_HI + 15 * 16 + 8],
            &115u64.to_le_bytes()
        );
        assert_eq!(&a[MXCSR..MXCSR + 4], &0x1f81u32.to_le_bytes());
        assert_eq!(xstate_bv(&a), XSTATE_SSE | XSTATE_AVX);
        // Without AVX in XCR0 the high halves are not written and the bit
        // is not set: XRSTOR would fault on it.
        let mut a = area();
        write_vector_regs(&mut a, 0b11, &buf).unwrap();
        assert_eq!(&a[YMM_HI..YMM_HI + 8], &[0; 8]);
        assert_eq!(xstate_bv(&a), XSTATE_SSE);
    }

    #[test]
    fn an_mxcsr_with_bits_the_cpu_lacks_is_invalid_args_and_writes_nothing() {
        let mut a = area();
        let before = a;
        let mut buf = [0u8; VECTOR_REGS_SIZE];
        buf[0..8].copy_from_slice(&[1; 8]);
        buf[VECTOR_MXCSR..VECTOR_MXCSR + 4].copy_from_slice(&0x1_0000u32.to_le_bytes());
        assert_eq!(
            write_vector_regs(&mut a, ALL, &buf),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(a, before);
        // With no mask recorded, the architectural default (0xffbf) decides:
        // bit 6 (DAZ) is outside it.
        a[MXCSR_MASK..MXCSR_MASK + 4].copy_from_slice(&0u32.to_le_bytes());
        buf[VECTOR_MXCSR..VECTOR_MXCSR + 4].copy_from_slice(&0x40u32.to_le_bytes());
        assert_eq!(
            write_vector_regs(&mut a, ALL, &buf),
            Err(ZxError::INVALID_ARGS)
        );
        buf[VECTOR_MXCSR..VECTOR_MXCSR + 4].copy_from_slice(&0xffbfu32.to_le_bytes());
        assert_eq!(write_vector_regs(&mut a, ALL, &buf), Ok(()));
    }

    #[test]
    fn single_step_is_the_trap_flag_and_only_zero_or_one() {
        let mut rflags = 0x246;
        let mut out = [0xaau8; 8];
        assert_eq!(read_single_step(rflags, &mut out), Ok(4));
        assert_eq!(&out[..4], &[0; 4]);
        assert_eq!(out[4], 0xaa);
        assert_eq!(write_single_step(&mut rflags, &1u32.to_le_bytes()), Ok(()));
        assert_eq!(rflags, 0x346);
        read_single_step(rflags, &mut out).unwrap();
        assert_eq!(&out[..4], &1u32.to_le_bytes());
        assert_eq!(
            write_single_step(&mut rflags, &2u32.to_le_bytes()),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            write_single_step(&mut rflags, &u32::MAX.to_le_bytes()),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(rflags, 0x346, "a refused write changes nothing");
        assert_eq!(write_single_step(&mut rflags, &0u32.to_le_bytes()), Ok(()));
        assert_eq!(rflags, 0x246);
        // A buffer may be larger than the value, never smaller.
        assert_eq!(write_single_step(&mut rflags, &[1, 0, 0, 0, 9]), Ok(()));
        assert_eq!(
            write_single_step(&mut rflags, &[0, 0, 0]),
            Err(ZxError::BUFFER_TOO_SMALL)
        );
        assert_eq!(
            read_single_step(rflags, &mut out[..3]),
            Err(ZxError::BUFFER_TOO_SMALL)
        );
    }

    #[test]
    fn short_buffers_are_buffer_too_small() {
        let mut a = area();
        let mut out = [0u8; VECTOR_REGS_SIZE];
        assert_eq!(
            read_fp_regs(&a, ALL, &mut out[..FP_REGS_SIZE - 1]),
            Err(ZxError::BUFFER_TOO_SMALL)
        );
        assert_eq!(
            write_fp_regs(&mut a, ALL, &out[..FP_REGS_SIZE - 1]),
            Err(ZxError::BUFFER_TOO_SMALL)
        );
        assert_eq!(
            read_vector_regs(&a, ALL, &mut out[..VECTOR_REGS_SIZE - 1]),
            Err(ZxError::BUFFER_TOO_SMALL)
        );
        assert_eq!(
            write_vector_regs(&mut a, ALL, &out[..VECTOR_REGS_SIZE - 1]),
            Err(ZxError::BUFFER_TOO_SMALL)
        );
    }

    #[test]
    fn a_rip_past_the_address_space_or_a_non_canonical_base_is_refused() {
        const END: usize = 0x40_0000_0000; // a 64 GiB hosted window's end
        let user = 0x2_0000_1234;
        assert_eq!(addresses_are_writable(user, 0, 0, END), Ok(()));
        assert_eq!(addresses_are_writable(0, END - 1, END - 8, END), Ok(()));
        // The base the hosted kernel lends a thread with no TLS: the host's,
        // past the window, canonical. A kernel-half base is canonical too.
        assert_eq!(
            addresses_are_writable(user, 0x7f60_49ff_d708, 0xffff_8000_0000_0000, END),
            Ok(())
        );
        // The first address past the window, the lowest non-canonical one,
        // and a kernel one: not for rip.
        for bad in [END, 0x8000_0000_0000, 0xffff_ff80_0000_0000] {
            assert_eq!(
                addresses_are_writable(bad, 0, 0, END),
                Err(ZxError::INVALID_ARGS)
            );
        }
        // Non-canonical bases.
        for bad in [0x8000_0000_0000, 0xdead_0000_0000_0000] {
            assert_eq!(
                addresses_are_writable(user, bad, 0, END),
                Err(ZxError::INVALID_ARGS)
            );
            assert_eq!(
                addresses_are_writable(user, 0, bad, END),
                Err(ZxError::INVALID_ARGS)
            );
        }
    }

    #[test]
    fn a_thread_cannot_turn_its_own_interrupts_off() {
        // IF (bit 9) and the reserved always-one bit 1 stay the kernel's;
        // the arithmetic flags, DF, TF, NT, AC and ID are the thread's.
        assert_eq!(user_rflags(0x202, 0), 0x202);
        assert_eq!(user_rflags(0x202, usize::MAX), 0x202 | X86_FLAGS_USER);
        assert_eq!(user_rflags(0x202, 0x24_4dd5), 0x24_4dd5 | 0x202);
        assert_eq!(
            X86_FLAGS_USER,
            1 | (1 << 2)
                | (1 << 4)
                | (1 << 6)
                | (1 << 7)
                | (1 << 8)
                | (1 << 10)
                | (1 << 11)
                | (1 << 14)
                | (1 << 18)
                | (1 << 21),
            "CF PF AF ZF SF TF DF OF NT AC ID"
        );
        // IOPL, VM, RF, VIF, VIP: not for the thread.
        assert_eq!(user_rflags(0x202, 0x3000 | 0x20000 | 0x10000), 0x202);
    }
}
