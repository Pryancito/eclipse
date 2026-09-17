//! In-kernel symbol table: turn a raw kernel address into `name+0x…`.
//!
//! Every crash reporter here — `[kfault-bt]`, `[kchain]`, `[heap-reentrant]`,
//! `[double-alloc]`, the panic RIP — used to print bare addresses and tell the
//! reader to run `llvm-addr2line`. That only works for whoever holds the exact
//! ELF that produced the log, and **two builds of the same commit on different
//! machines do not share a `.text` layout**. A log symbolized against the wrong
//! kernel does not fail: it confidently names the wrong functions. That cost
//! this hunt more than one wrong turn.
//!
//! So the kernel carries its own table. [`KSYMS`] reserves a fixed-size,
//! zero-filled section in the image; after the link, `tools/gen_ksyms.py`
//! reads the ELF's own symbol table and patches the blob into that section
//! **in place** with `llvm-objcopy --update-section`. Because the reservation
//! never changes size, nothing shifts and no second link is needed.
//!
//! Everything degrades to today's behaviour if the blob was never patched (bad
//! magic → [`lookup`] returns `None` → the reporters print the bare address),
//! so a build without the post-link step still boots and still reports.
//!
//! Layout, little-endian, all offsets in bytes from the start of the blob:
//!
//! ```text
//!   u32 magic = KSYM_MAGIC        u32 version = 1
//!   u32 count                     u32 strtab_off
//!   u64 image_base                u64 reserved
//!   count × { u32 addr_off /* from KERNEL_BEGIN */, u32 name_off /* from strtab_off */ }
//!   strtab: NUL-terminated names, already demangled and truncated
//! ```
//!
//! A symbol's extent is "up to the next symbol's address": explicit sizes would
//! cost another 4 bytes per entry to sharpen a diagnostic that only needs to
//! name the function. [`MAX_SYM_SPAN`] rejects an offset too large to be a real
//! one, so an address past the last symbol is reported as an address, not as
//! `last_symbol+0x3f2a10`.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, Ordering};

/// `"KSYM"`.
const KSYM_MAGIC: u32 = 0x4d59_534b;
const KSYM_VERSION: u32 = 1;
const HEADER_LEN: usize = 32;

/// Largest `addr - symbol_start` still reported as belonging to that symbol.
/// No single kernel function is a megabyte long, so a larger offset means the
/// address is past the end of the table (or in a section the table does not
/// cover) and naming it would be a lie.
const MAX_SYM_SPAN: u64 = 1 << 20;

/// Reserved size of the patched-in blob. The post-link step fails loudly if the
/// real table does not fit, so this only ever needs raising, never guessing:
/// ~18.5k symbols with demangled names truncated to 80 chars measured ~0.94 MB.
pub const KSYMS_CAP: usize = 2 * 1024 * 1024;

/// The reservation itself.
///
/// Two requirements shape this declaration, and missing either makes the table
/// silently invisible at runtime:
///
///  * it must land in a **PROGBITS** section for `llvm-objcopy
///    --update-section` to write it. An all-zero initializer would be placed in
///    `.bss` (NOBITS), hence the non-zero first bytes — which double as a
///    deliberately invalid magic, so an unpatched kernel reads as "no table";
///  * it must be **`UnsafeCell`**, and read through `read_volatile`. A plain
///    immutable `static` is `constant` to LLVM, which then proves from the
///    initializer that the magic can never match and deletes the whole
///    lookup — observed exactly once, as a kernel that reported "no in-kernel
///    symbol table" while carrying 18351 of them. Interior mutability drops
///    the attribute; the volatile read keeps the loads from being folded back.
///
/// Nothing in the kernel ever writes it: the only writer is the post-link
/// patch, long before the image runs.
#[repr(C, align(8))]
struct Blob(UnsafeCell<[u8; KSYMS_CAP]>);

// SAFETY: written only by the post-link patch step, read-only at runtime.
unsafe impl Sync for Blob {}

#[used]
#[unsafe(link_section = ".ksyms")]
static KSYMS: Blob = Blob(UnsafeCell::new({
    let mut blob = [0u8; KSYMS_CAP];
    blob[0] = 0xff;
    blob[1] = 0xff;
    blob[2] = 0xff;
    blob[3] = 0xff;
    blob
}));

#[inline]
fn blob_ptr() -> *const u8 {
    KSYMS.0.get() as *const u8
}

/// Cached verdict on the blob's header: 0 unchecked, 1 usable, 2 unusable.
/// Only an optimization — the parse is a handful of loads — but crash paths run
/// with the console lock held and every cycle there is one the machine spends
/// not printing.
static STATE: AtomicU8 = AtomicU8::new(0);

#[inline]
fn u32_at(off: usize) -> u32 {
    if off + 4 > KSYMS_CAP {
        return 0;
    }
    // SAFETY: bounds checked above. Byte-wise so no alignment is assumed, and
    // volatile so the loads survive optimization (see [`Blob`]).
    unsafe {
        let p = blob_ptr().add(off);
        u32::from_le_bytes([
            core::ptr::read_volatile(p),
            core::ptr::read_volatile(p.add(1)),
            core::ptr::read_volatile(p.add(2)),
            core::ptr::read_volatile(p.add(3)),
        ])
    }
}

#[inline]
fn byte_at(off: usize) -> u8 {
    if off >= KSYMS_CAP {
        return 0;
    }
    // SAFETY: bounds checked above; volatile for the reason in [`Blob`].
    unsafe { core::ptr::read_volatile(blob_ptr().add(off)) }
}

/// `(count, strtab_off, image_base)` when the blob holds a table this kernel
/// understands.
///
/// The base is carried in the blob rather than hardcoded so the table is not
/// tied to one architecture's `KERNEL_BEGIN`: the generator subtracts whatever
/// base the image was linked at and says so here.
fn header() -> Option<(usize, usize, u64)> {
    match STATE.load(Ordering::Relaxed) {
        1 => {}
        2 => return None,
        _ => {
            let ok = u32_at(0) == KSYM_MAGIC && u32_at(4) == KSYM_VERSION;
            STATE.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
            if !ok {
                return None;
            }
        }
    }
    let count = u32_at(8) as usize;
    let strtab_off = u32_at(12) as usize;
    let image_base = (u32_at(16) as u64) | ((u32_at(20) as u64) << 32);
    // A blob patched by a mismatched generator must not be able to walk us off
    // the end of the array.
    if strtab_off > KSYMS_CAP || HEADER_LEN + count.checked_mul(8)? > strtab_off {
        STATE.store(2, Ordering::Relaxed);
        return None;
    }
    Some((count, strtab_off, image_base))
}

#[inline]
fn entry_addr(base: u64, i: usize) -> u64 {
    base + u32_at(HEADER_LEN + i * 8) as u64
}

fn name_at(strtab_off: usize, name_off: usize) -> Option<&'static str> {
    let start = strtab_off.checked_add(name_off)?;
    if start >= KSYMS_CAP {
        return None;
    }
    let mut len = 0usize;
    while start + len < KSYMS_CAP && byte_at(start + len) != 0 {
        len += 1;
    }
    if len == 0 || start + len >= KSYMS_CAP {
        return None;
    }
    // SAFETY: `start..start + len` is in bounds and holds no NUL; the blob is
    // read-only at runtime, so the slice cannot change under us.
    let bytes = unsafe { core::slice::from_raw_parts(blob_ptr().add(start), len) };
    core::str::from_utf8(bytes).ok()
}

/// The symbol containing `addr`, as `(name, offset_into_symbol)`.
///
/// `None` when there is no table, when `addr` is outside the kernel image, or
/// when the nearest symbol starts more than [`MAX_SYM_SPAN`] below it.
pub fn lookup(addr: u64) -> Option<(&'static str, u64)> {
    let (count, strtab_off, base) = header()?;
    if count == 0 || addr < entry_addr(base, 0) {
        return None;
    }
    // Last entry whose address is <= addr.
    let (mut lo, mut hi) = (0usize, count - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if entry_addr(base, mid) <= addr {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let off = addr - entry_addr(base, lo);
    // A symbol reaches at most to the next one. Without that bound a label at
    // a section boundary claims everything after it: `_copy_user_end` sits on
    // the image base (the `.text.copy_user` region is declared but empty) and
    // was naming low addresses in every backtrace this hunt produced.
    // `MAX_SYM_SPAN` only backstops the very last entry.
    let span = if lo + 1 < count {
        entry_addr(base, lo + 1).saturating_sub(entry_addr(base, lo))
    } else {
        MAX_SYM_SPAN
    };
    if off >= span.min(MAX_SYM_SPAN) {
        return None;
    }
    let name_off = u32_at(HEADER_LEN + lo * 8 + 4) as usize;
    Some((name_at(strtab_off, name_off)?, off))
}

/// A kernel address that prints as `0x…` alone, or as `0x… <name+0x…>` when the
/// symbol table can name it.
///
/// Every crash reporter formats addresses through this, so adding the table
/// took one `Display` impl rather than an edit per call site.
pub struct Addr(pub u64);

impl core::fmt::Display for Addr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#x}", self.0)?;
        if let Some((name, off)) = lookup(self.0) {
            if off == 0 {
                write!(f, " <{}>", name)?;
            } else {
                write!(f, " <{}+{:#x}>", name, off)?;
            }
        }
        Ok(())
    }
}

/// Whether a usable table was patched into this kernel, for the one-line note
/// the crash reporters print when it is missing (so nobody wonders why the
/// backtrace has no names).
pub fn available() -> bool {
    header().is_some()
}
