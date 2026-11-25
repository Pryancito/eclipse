//! A driver's own ioctl, over the link ([`op::IOCTL`](super::op::IOCTL)).
//!
//! The argument of an ioctl is a pointer into the caller's memory, which
//! the driver's process cannot read. So the kernel's end copies what the
//! driver will read: the struct, `_IOC_SIZE` of the command bytes, and
//! the arrays the struct points at, for the few ioctls whose struct has
//! them ([`NESTED`]). The driver's end puts each in a buffer of its own,
//! points the struct's fields at them, and calls the driver as the kernel
//! would have, with a pointer to its copy of the struct. The struct comes
//! back, with the pointers it had, and the kernel's end writes it over
//! the caller's if the driver changed it.
//!
//! The arrays only go in: the driver reads them and writes nothing back
//! into them (`user_slice_ok`'s and `GEM_PUSHBUF`'s readers in
//! `display::nvidia`).
//!
//! The message: `a` is the command, `b` the caller's pid; the bytes are
//! the DRM file (u64), the argument word (u64, the driver's when the
//! command carries no struct), the struct, then each array of [`NESTED`]
//! with a count, in its order. The answer's bytes are the result (i64, an
//! `errno` negated when below 0) and the struct after the call.

use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryInto;

use super::{DrmScheme, Transport, EINVAL};

/// `errno` for an argument that is not where the caller said.
pub const EFAULT: i32 = 14;
/// `errno` for an argument too big for one message.
pub const E2BIG: i32 = 7;

/// An array an ioctl's struct points at: the offsets in the struct of its
/// count (u32) and its pointer (u64), and the size of one element.
#[derive(Clone, Copy, Debug)]
pub struct Array {
    pub count_at: usize,
    pub ptr_at: usize,
    pub elem: usize,
}

const fn array(count_at: usize, ptr_at: usize, elem: usize) -> Array {
    Array {
        count_at,
        ptr_at,
        elem,
    }
}

/// `struct drm_nouveau_sync` (16 bytes), the waits and signals of an EXEC
/// and of a VM_BIND.
const SYNC: usize = 16;

/// The driver-private ioctls whose struct points at arrays, by DRM `nr`,
/// with them in the order they travel. The layouts are nouveau's uapi
/// (`nouveau_drm.h`), and a test in `display` holds them against the
/// driver's own structs.
pub const NESTED: &[(u32, &[Array])] = &[
    // DRM_NOUVEAU_VM_BIND: op_count@0, wait_count@8, sig_count@12,
    // wait_ptr@16, sig_ptr@24, op_ptr@32; `drm_nouveau_vm_bind_op` is 40.
    (
        0x40 + 0x11,
        &[array(0, 32, 40), array(8, 16, SYNC), array(12, 24, SYNC)],
    ),
    // DRM_NOUVEAU_EXEC: push_count@4, wait_count@8, sig_count@12,
    // wait_ptr@16, sig_ptr@24, push_ptr@32; `drm_nouveau_exec_push` is 16.
    (
        0x40 + 0x12,
        &[array(4, 32, 16), array(8, 16, SYNC), array(12, 24, SYNC)],
    ),
    // DRM_NOUVEAU_GEM_PUSHBUF: nr_buffers@4, buffers@8, nr_push@20,
    // push@32; `drm_nouveau_gem_pushbuf_bo` is 40, `_push` 24. The relocs
    // are not read by the driver, so they stay where they are.
    (0x40 + 0x41, &[array(4, 8, 40), array(20, 32, 24)]),
];

/// The arrays of command `cmd`'s struct, none for most.
pub fn nested(cmd: u32) -> &'static [Array] {
    let nr = cmd & 0xff;
    // Only in the driver-private range of DRM's type ('d').
    if (cmd >> 8) & 0xff != 0x64 {
        return &[];
    }
    NESTED
        .iter()
        .find(|(n, _)| *n == nr)
        .map(|(_, a)| *a)
        .unwrap_or(&[])
}

/// `_IOC_SIZE(cmd)`.
pub fn ioc_size(cmd: u32) -> usize {
    ((cmd >> 16) & 0x3fff) as usize
}

fn u32_at(s: &[u8], at: usize) -> Option<u32> {
    s.get(at..at + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

fn u64_at(s: &[u8], at: usize) -> Option<u64> {
    s.get(at..at + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

fn put_u64(s: &mut [u8], at: usize, v: u64) {
    s[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// The bytes of array `a` of `top`: how many, and where in the caller.
fn extent(top: &[u8], a: &Array) -> Option<(usize, u64)> {
    let count = u32_at(top, a.count_at)? as usize;
    let ptr = u64_at(top, a.ptr_at)?;
    Some((count.checked_mul(a.elem)?, ptr))
}

/// The kernel's end: ioctl `cmd` on `arg`, by the caller `pid` on DRM file
/// `file`, sent to the driver over `to`.
pub fn call<T: Transport>(
    to: &T,
    send: impl FnOnce(&[u8]) -> Result<Vec<u8>, i32>,
    cmd: u32,
    arg: usize,
    file: usize,
) -> Result<usize, i32> {
    let size = ioc_size(cmd);
    let mut msg = Vec::with_capacity(16 + size);
    msg.extend_from_slice(&(file as u64).to_le_bytes());
    msg.extend_from_slice(&(arg as u64).to_le_bytes());
    let top = if size > 0 {
        to.read_user(arg, size)?
    } else {
        Vec::new()
    };
    msg.extend_from_slice(&top);
    for a in nested(cmd) {
        let (len, ptr) = extent(&top, a).ok_or(EINVAL)?;
        if len > 0 {
            if ptr == 0 {
                return Err(EFAULT);
            }
            msg.extend_from_slice(&to.read_user(ptr as usize, len)?);
        }
    }
    let back = send(&msg)?;
    let ret = u64_at(&back, 0).ok_or(EINVAL)? as i64;
    let after = &back[8..];
    if after.len() != top.len() {
        return Err(EINVAL);
    }
    // What the driver changed goes back to the caller, as its own writes
    // into the caller's struct did; a struct it left alone is not written,
    // so an `_IOW` argument in read-only memory stays as good as it was.
    if after != &top[..] {
        to.write_user(arg, after)?;
    }
    if ret < 0 {
        Err((-ret) as i32)
    } else {
        Ok(ret as usize)
    }
}

/// The driver's end: the message `data` of command `cmd` by `pid`, made
/// into a call on `gpu`, its answer put in `out`.
pub fn answer(gpu: &dyn DrmScheme, cmd: u32, pid: u64, data: &[u8], out: &mut Vec<u8>) {
    let ret: i64 = match run(gpu, cmd, pid, data) {
        Ok((ret, top)) => {
            out.extend_from_slice(&ret.to_le_bytes());
            out.extend_from_slice(&top);
            return;
        }
        Err(errno) => -(errno as i64),
    };
    // A message that did not parse: the struct goes back as it came, so
    // the caller's is not written.
    out.extend_from_slice(&ret.to_le_bytes());
    let size = ioc_size(cmd);
    out.extend_from_slice(data.get(16..16 + size).unwrap_or(&vec![0; size]));
}

/// Bytes in buffers of u64s, so the driver's `&*(arg as *const T)` finds
/// them aligned as a caller's struct is.
fn aligned(bytes: &[u8]) -> Vec<u64> {
    let mut buf = vec![0u64; bytes.len().div_ceil(8).max(1)];
    // SAFETY: `buf` holds at least `bytes.len()` bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.as_mut_ptr() as *mut u8, bytes.len());
    }
    buf
}

fn run(gpu: &dyn DrmScheme, cmd: u32, pid: u64, data: &[u8]) -> Result<(i64, Vec<u8>), i32> {
    let size = ioc_size(cmd);
    let file = u64_at(data, 0).ok_or(EINVAL)? as usize;
    let arg = u64_at(data, 8).ok_or(EINVAL)? as usize;
    let top = data.get(16..16 + size).ok_or(EINVAL)?;
    let mut rest = &data[16 + size..];
    let mut mine = aligned(top);
    let mut arrays = Vec::new();
    {
        // SAFETY: `mine` holds at least `size` bytes.
        let view = unsafe { core::slice::from_raw_parts_mut(mine.as_mut_ptr() as *mut u8, size) };
        for a in nested(cmd) {
            let (len, _) = extent(top, a).ok_or(EINVAL)?;
            if len == 0 {
                continue;
            }
            if rest.len() < len {
                return Err(EINVAL);
            }
            let (here, more) = rest.split_at(len);
            rest = more;
            let buf = aligned(here);
            put_u64(view, a.ptr_at, buf.as_ptr() as u64);
            arrays.push(buf);
        }
    }
    if !rest.is_empty() {
        return Err(EINVAL);
    }
    let at = if size > 0 {
        mine.as_mut_ptr() as usize
    } else {
        arg
    };
    let ret = match gpu.ioctl_file(cmd, at, pid, file) {
        Ok(v) => v as i64,
        Err(errno) => -(errno as i64),
    };
    drop(arrays);
    // SAFETY: as above.
    let mut after =
        unsafe { core::slice::from_raw_parts(mine.as_ptr() as *const u8, size) }.to_vec();
    // The caller's pointers, not the copies'.
    for a in nested(cmd) {
        put_u64(&mut after, a.ptr_at, u64_at(top, a.ptr_at).ok_or(EINVAL)?);
    }
    Ok((ret, after))
}
