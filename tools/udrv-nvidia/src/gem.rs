//! The kernel's table of mappable GEM objects, for the driver
//! (`drivers_gem`, which `zcore-drivers` calls with its `process`
//! feature instead of keeping the table itself): each call goes to the
//! kernel (`ECLIPSE_GEM`), where the mmap, PRIME and KMS paths read it.
use udrivers::zx::{self, nr, Status};

/// The `errno` of a status the kernel answered: what the table's callers
/// tell apart, a refusal and a handle that is not there.
fn errno_of(s: Status) -> i64 {
    match s {
        Status::ACCESS_DENIED => libc::EPERM as i64,
        Status::NOT_FOUND => libc::ENOENT as i64,
        _ => libc::EINVAL as i64,
    }
}

#[no_mangle]
extern "C" fn drivers_gem(op: u32, args: *const u64, out: *mut u64, _out_len: usize) -> i64 {
    // SAFETY: `zcore-drivers` passes four words at `args`, and room at
    // `out` for what op `op` answers (the fourth word says how much, for
    // a list), which the kernel writes and nothing more.
    let a = unsafe { std::slice::from_raw_parts(args, 4) };
    let call = [
        op as usize,
        a[0] as usize,
        a[1] as usize,
        a[2] as usize,
        a[3] as usize,
        out as usize,
    ];
    // SAFETY: as above.
    match unsafe { zx::call(nr::ECLIPSE_GEM, call) } {
        Ok(v) => v as i64,
        Err(s) => -errno_of(s),
    }
}
