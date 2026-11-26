//! The kernel's table of sync objects and buffer reservations, for the
//! driver (`drivers_syncobj`, which `zcore-drivers` calls with its
//! `process` feature instead of keeping the tables itself): each call goes
//! to the kernel (`ECLIPSE_SYNCOBJ`), where its clients' syncobjs are.
use udrivers::zx::{self, nr, Status};

/// The `errno` of a status the kernel answered.
fn errno_of(s: Status) -> i64 {
    match s {
        Status::ACCESS_DENIED => libc::EPERM as i64,
        Status::NO_RESOURCES => libc::ENOSPC as i64,
        Status::OUT_OF_RANGE => libc::E2BIG as i64,
        _ => libc::EINVAL as i64,
    }
}

#[no_mangle]
extern "C" fn drivers_syncobj(
    op: u32,
    inp: *const u64,
    in_len: usize,
    out: *mut u64,
    out_len: usize,
) -> i64 {
    let call = [op as usize, inp as usize, in_len, out as usize, out_len, 0];
    // SAFETY: `zcore-drivers` passes `in_len` words at `inp` and room for
    // `out_len` at `out`, which the kernel reads and writes and nothing
    // more.
    match unsafe { zx::call(nr::ECLIPSE_SYNCOBJ, call) } {
        Ok(v) => v as i64,
        Err(s) => -errno_of(s),
    }
}
