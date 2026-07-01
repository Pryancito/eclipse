//! Implementation of the small subset of NVIDIA's internal "OBJOS" service
//! surface that real vendored RM files (core/thread_state.c, the nvport
//! libraries, diagnostics/nvlog_printf.c) call directly -- distinct from
//! the `os-interface.h` ABI in `os_interface.rs`. Real signatures
//! transcribed from src/nvidia/generated/g_os_nvoc.h (MIT,
//! NVIDIA/open-gpu-kernel-modules); NVIDIA's own real implementation of
//! these lives in the Linux-specific arch/nvalloc/unix/src tree, which
//! Eclipse has no equivalent of, so each is backed by Eclipse's own
//! primitives (via `crate::hooks`) or a safe default, same convention as
//! `os_interface.rs`.
//!
//! Also holds a handful of standalone placeholders for RM-internal
//! symbols outside both ABIs that the same vendored files reference
//! unconditionally, but that belong to subsystems (OBJSYS, OBJGPUMGR,
//! the RCDB crash journal, OS thread-priority boosting) not vendored yet.
#![allow(non_snake_case)]

use crate::hooks::with_hooks;
use crate::types::*;

#[no_mangle]
pub extern "C" fn osGetCurrentThread(handle: *mut NvU64) -> NV_STATUS {
    if handle.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe { *handle = 0 };
    NV_OK
}

#[no_mangle]
pub extern "C" fn osGetCurrentProcessorNumber() -> NvU32 {
    0
}

#[no_mangle]
pub extern "C" fn osGetMaximumCoreCount() -> NvU32 {
    1
}

#[no_mangle]
pub extern "C" fn osGetCurrentProcessFlags() -> NvU32 {
    0
}

#[no_mangle]
pub extern "C" fn osDelayUs(microseconds: NvU32) -> NV_STATUS {
    with_hooks((), |h| h.delay_us(microseconds));
    NV_OK
}

#[no_mangle]
pub extern "C" fn osGetMonotonicTimeNs() -> NvU64 {
    with_hooks(0, |h| h.monotonic_time_ns())
}

#[no_mangle]
pub extern "C" fn osGetTimeoutParams(
    _gpu: *mut c_void,
    time_out_us: *mut NvU32,
    scale: *mut NvU32,
    flags: *mut NvU32,
) {
    unsafe {
        if !time_out_us.is_null() {
            *time_out_us = 2_000_000;
        }
        if !scale.is_null() {
            *scale = 1;
        }
        if !flags.is_null() {
            *flags = 0;
        }
    }
}

#[no_mangle]
pub extern "C" fn osSchedule() -> NV_STATUS {
    NV_OK
}

#[no_mangle]
pub extern "C" fn osReadRegistryDword(
    _gpu: *mut c_void,
    _name: *const c_char,
    data: *mut NvU32,
) -> NV_STATUS {
    if !data.is_null() {
        unsafe { *data = 0 };
    }
    NV_ERR_OPERATING_SYSTEM
}

#[no_mangle]
pub extern "C" fn osReadRegistryString(
    _gpu: *mut c_void,
    _name: *const c_char,
    _data: *mut NvU8,
    length: *mut NvU32,
) -> NV_STATUS {
    if !length.is_null() {
        unsafe { *length = 0 };
    }
    NV_ERR_OPERATING_SYSTEM
}

#[no_mangle]
pub extern "C" fn osGetSystemTime(sec: *mut NvU32, usec: *mut NvU32) -> NV_STATUS {
    let ns = with_hooks(0u64, |h| h.monotonic_time_ns());
    unsafe {
        if !sec.is_null() {
            *sec = (ns / 1_000_000_000) as NvU32;
        }
        if !usec.is_null() {
            *usec = ((ns / 1_000) % 1_000_000) as NvU32;
        }
    }
    NV_OK
}

/// The real `struct OBJSYS` singleton, normally constructed by
/// src/nvidia/src/kernel/core/system.c (not vendored yet). NULL until
/// that subsystem is brought up; nothing vendored this pass dereferences
/// it, only checks it for NULL defensively.
#[no_mangle]
pub static mut g_pSys: *mut c_void = core::ptr::null_mut();

#[no_mangle]
pub extern "C" fn gpumgrGetCurrentGpuInstance() -> NvU32 {
    0
}

#[no_mangle]
pub extern "C" fn threadPriorityStateAlloc() {}
#[no_mangle]
pub extern "C" fn threadPriorityStateFree() {}

#[no_mangle]
pub extern "C" fn rcdbAddAssertJournalRecWithLine(
    _gpu: *mut c_void,
    _line_num: NvU32,
    _rec: *mut *mut c_void,
    _group: NvU8,
    _rec_type: NvU8,
    _size: NvU16,
    _level: NvU32,
    _key: NvU64,
) -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}
