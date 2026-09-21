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

/// The symbolizer had no tests, on the one path whose failure mode this
/// module's own header warns about: it does not fail, it confidently names
/// the wrong function. Every crash report in the kernel reads through here.
///
/// The tests build the blob byte for byte in the layout at the top of this
/// file and patch it in, so they fail if this code and `tools/gen_ksyms.py`
/// ever stop agreeing on the format — which is the only thing holding the two
/// halves together, since nothing else in the tree parses it.
///
/// One thing to know before sharpening [`lookup`]. Of the two halves of its
/// extent check, only [`MAX_SYM_SPAN`] can reject anything on a sorted table:
/// the search returns the *last* entry at or below `addr`, so the offset is
/// already smaller than the distance to the next entry, and that half is
/// unreachable — it is a backstop for a table that arrives out of order. The
/// megabyte cap is what does the work, and it does it in two places: past the
/// last symbol, and inside a gap wider than a megabyte between two of them
/// (a section boundary, which is where `_copy_user_end` used to claim every
/// low address in this hunt's backtraces). Both are pinned below.
#[cfg(test)]
mod ksyms_tests {
    use super::*;
    use alloc::vec::Vec;

    /// One blob, one cached verdict, shared by every test here.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write `bytes` where the post-link step writes, and drop the cached
    /// header verdict so the next lookup re-reads it.
    fn install(bytes: &[u8]) {
        assert!(bytes.len() <= KSYMS_CAP);
        // SAFETY: `KSYMS` is an `UnsafeCell` and these tests hold `test_lock`,
        // so nothing else is reading it. This is what `llvm-objcopy
        // --update-section` does to the linked image.
        unsafe {
            let p = KSYMS.0.get() as *mut u8;
            core::ptr::write_bytes(p, 0, KSYMS_CAP);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        }
        STATE.store(0, Ordering::Relaxed);
    }

    /// The generator's layout: header, `(addr_off, name_off)` pairs, strtab.
    fn blob(base: u64, syms: &[(u32, &str)]) -> Vec<u8> {
        let mut strtab: Vec<u8> = Vec::new();
        let mut entries: Vec<(u32, u32)> = Vec::new();
        for (addr_off, name) in syms {
            let name_off = strtab.len() as u32;
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
            entries.push((*addr_off, name_off));
        }
        let strtab_off = (HEADER_LEN + entries.len() * 8) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&KSYM_MAGIC.to_le_bytes());
        out.extend_from_slice(&KSYM_VERSION.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&strtab_off.to_le_bytes());
        out.extend_from_slice(&base.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(out.len(), HEADER_LEN);
        for (a, n) in entries {
            out.extend_from_slice(&a.to_le_bytes());
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(&strtab);
        out
    }

    const BASE: u64 = 0xffff_ff00_0000_0000;

    /// Three symbols at 0x1000, 0x2000 and 0x3000 from the image base.
    fn three_symbols() {
        install(&blob(
            BASE,
            &[
                (0x1000, "arranca"),
                (0x2000, "sirve_syscall"),
                (0x3000, "panica"),
            ],
        ));
    }

    // ── the header ─────────────────────────────────────────────────────────

    #[test]
    fn a_kernel_whose_table_was_never_patched_reports_no_table() {
        // The reservation ships with a deliberately invalid magic so an image
        // that skipped the post-link step still boots and still reports — with
        // bare addresses, which is what it did before the table existed.
        let _g = test_lock();
        install(&[0xff, 0xff, 0xff, 0xff]);
        assert!(!available());
        assert!(lookup(BASE + 0x1000).is_none());
    }

    #[test]
    fn a_table_from_another_format_is_refused_rather_than_parsed() {
        let _g = test_lock();
        let good = blob(BASE, &[(0x1000, "arranca")]);

        let mut wrong_magic = good.clone();
        wrong_magic[0] ^= 0xff;
        install(&wrong_magic);
        assert!(!available(), "a bad magic is not a table");

        let mut wrong_version = good.clone();
        wrong_version[4..8].copy_from_slice(&(KSYM_VERSION + 1).to_le_bytes());
        install(&wrong_version);
        assert!(!available(), "a future version is not this version");

        install(&good);
        assert!(available(), "and the real thing is accepted");
    }

    #[test]
    fn a_string_table_that_overlaps_the_entries_is_refused() {
        // A mismatched generator must not be able to walk the lookup off the
        // end of the reservation, or into its own index.
        let _g = test_lock();
        let mut b = blob(BASE, &[(0x1000, "arranca"), (0x2000, "para")]);
        // Claim the strtab starts inside the entry array.
        b[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        install(&b);
        assert!(!available());

        let mut b = blob(BASE, &[(0x1000, "arranca")]);
        b[12..16].copy_from_slice(&(KSYMS_CAP as u32 + 1).to_le_bytes());
        install(&b);
        assert!(!available());
    }

    #[test]
    fn an_empty_table_names_nothing() {
        let _g = test_lock();
        install(&blob(BASE, &[]));
        assert!(available(), "an empty table is still a well-formed table");
        assert!(lookup(BASE).is_none());
        assert!(lookup(BASE + 0x1000).is_none());
    }

    // ── the lookup ─────────────────────────────────────────────────────────

    #[test]
    fn an_address_on_a_symbol_is_named_with_offset_zero() {
        let _g = test_lock();
        three_symbols();
        assert_eq!(lookup(BASE + 0x1000), Some(("arranca", 0)));
        assert_eq!(lookup(BASE + 0x2000), Some(("sirve_syscall", 0)));
        assert_eq!(lookup(BASE + 0x3000), Some(("panica", 0)));
    }

    #[test]
    fn an_address_inside_a_symbol_carries_its_offset() {
        let _g = test_lock();
        three_symbols();
        assert_eq!(lookup(BASE + 0x1004), Some(("arranca", 4)));
        assert_eq!(lookup(BASE + 0x1fff), Some(("arranca", 0xfff)));
        assert_eq!(lookup(BASE + 0x2abc), Some(("sirve_syscall", 0xabc)));
    }

    #[test]
    fn the_first_byte_of_a_symbol_belongs_to_it_and_not_to_its_neighbour() {
        // Off by one here means every return address that happens to land on a
        // function entry is attributed to the function before it — which in a
        // backtrace is precisely the caller, so the report looks plausible.
        let _g = test_lock();
        three_symbols();
        assert_eq!(lookup(BASE + 0x1fff), Some(("arranca", 0xfff)));
        assert_eq!(lookup(BASE + 0x2000), Some(("sirve_syscall", 0)));
    }

    #[test]
    fn an_address_below_the_first_symbol_is_not_named() {
        let _g = test_lock();
        three_symbols();
        assert!(lookup(BASE).is_none());
        assert!(lookup(BASE + 0xfff).is_none());
        assert!(lookup(0).is_none(), "a null pointer is not in the image");
    }

    #[test]
    fn past_the_last_symbol_only_a_megabyte_is_claimed() {
        // The last entry has no successor to bound it, so without
        // `MAX_SYM_SPAN` it would name every address above it — including the
        // heap and the user half. A report that says `panica+0x3f2a10` is
        // worse than one that says `0xffffff0040000000`.
        let _g = test_lock();
        three_symbols();
        let last = BASE + 0x3000;
        assert_eq!(
            lookup(last + MAX_SYM_SPAN - 1),
            Some(("panica", MAX_SYM_SPAN - 1))
        );
        assert!(lookup(last + MAX_SYM_SPAN).is_none());
        assert!(lookup(last + 0x4000_0000).is_none());
    }

    #[test]
    fn the_search_lands_on_the_right_symbol_across_a_large_table() {
        // Exercises the binary search itself rather than a handful of entries:
        // 1000 symbols, every one probed at its start, its middle and its last
        // byte. A table this size is what the kernel actually carries (~18k).
        let _g = test_lock();
        let names: Vec<alloc::string::String> =
            (0..1000).map(|i| alloc::format!("fn_{}", i)).collect();
        let syms: Vec<(u32, &str)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (0x1000 + i as u32 * 0x40, n.as_str()))
            .collect();
        install(&blob(BASE, &syms));
        for (i, name) in names.iter().enumerate() {
            let start = BASE + 0x1000 + i as u64 * 0x40;
            assert_eq!(lookup(start), Some((name.as_str(), 0)), "start of {}", name);
            assert_eq!(
                lookup(start + 0x20),
                Some((name.as_str(), 0x20)),
                "mid of {}",
                name
            );
            assert_eq!(
                lookup(start + 0x3f),
                Some((name.as_str(), 0x3f)),
                "end of {}",
                name
            );
        }
    }

    #[test]
    fn a_gap_wider_than_a_megabyte_is_not_attributed_to_the_symbol_before_it() {
        // Two symbols with a section boundary between them. The first does not
        // reach across it: a label sitting at the edge of an empty region
        // would otherwise name everything in the hole, which is exactly what
        // `_copy_user_end` did to every backtrace this hunt produced.
        let _g = test_lock();
        install(&blob(
            BASE,
            &[(0x1000, "borde"), (0x90_0000, "otra_seccion")],
        ));
        assert_eq!(
            lookup(BASE + 0x1000 + MAX_SYM_SPAN - 1),
            Some(("borde", MAX_SYM_SPAN - 1))
        );
        assert!(lookup(BASE + 0x1000 + MAX_SYM_SPAN).is_none());
        // The far side of the gap is named normally.
        assert_eq!(lookup(BASE + 0x90_0000), Some(("otra_seccion", 0)));
    }

    #[test]
    fn the_next_symbol_bound_is_a_backstop_for_an_unsorted_table() {
        // On a sorted table this bound is unreachable: the search returns the
        // last entry at or below `addr`, so the offset is already below the
        // span. Written down because a reader who believes otherwise will
        // "simplify" the search and find nothing complaining. What it does
        // cover is a table out of order, where the search's answer is
        // meaningless and only the spans keep the lie small.
        let _g = test_lock();
        three_symbols();
        for probe in [0x1000u64, 0x1234, 0x2000, 0x2fff, 0x3000] {
            let (_, off) = lookup(BASE + probe).expect("inside the table");
            assert!(
                off < 0x1000,
                "offset {:#x} exceeds the symbol's own span",
                off
            );
        }
    }

    // ── the names ──────────────────────────────────────────────────────────

    #[test]
    fn a_name_that_runs_to_the_end_without_a_terminator_is_not_returned() {
        // The strtab is inside a fixed reservation, so an unterminated last
        // name would otherwise be read as a slice reaching the end of it.
        let _g = test_lock();
        let mut b = blob(BASE, &[(0x1000, "arranca")]);
        b.pop(); // drop the trailing NUL
                 // Fill the rest of the reservation with non-NUL bytes so the scan
                 // cannot stop anywhere.
        b.resize(KSYMS_CAP, b'A');
        install(&b);
        assert!(lookup(BASE + 0x1000).is_none());
    }

    #[test]
    fn a_name_that_is_not_utf8_is_dropped_instead_of_panicking() {
        let _g = test_lock();
        let mut b = blob(BASE, &[(0x1000, "arranca")]);
        let strtab_off = HEADER_LEN + 8;
        b[strtab_off] = 0xff;
        install(&b);
        assert!(lookup(BASE + 0x1000).is_none());
    }

    #[test]
    fn a_name_offset_past_the_reservation_is_dropped() {
        let _g = test_lock();
        let mut b = blob(BASE, &[(0x1000, "arranca")]);
        // The entry's `name_off` sits at HEADER_LEN + 4.
        b[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        install(&b);
        assert!(lookup(BASE + 0x1000).is_none());
    }

    // ── what the reporters print ───────────────────────────────────────────

    #[test]
    fn addr_prints_the_number_first_and_the_name_only_if_there_is_one() {
        // Every crash reporter formats through `Addr`, so the address itself
        // must survive even when the table cannot name it — that number is
        // what `llvm-addr2line` still takes.
        let _g = test_lock();
        three_symbols();
        assert_eq!(
            alloc::format!("{}", Addr(BASE + 0x2000)),
            "0xffffff0000002000 <sirve_syscall>"
        );
        assert_eq!(
            alloc::format!("{}", Addr(BASE + 0x2abc)),
            "0xffffff0000002abc <sirve_syscall+0xabc>"
        );
        assert_eq!(alloc::format!("{}", Addr(BASE)), "0xffffff0000000000");
    }
}
