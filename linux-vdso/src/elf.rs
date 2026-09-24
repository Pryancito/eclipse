//! Symbol lookup in a mapped vDSO image, as a C library performs it.
//!
//! This is a port of musl's `__vdsosym` (`src/internal/vdso.c`), and it is
//! shared verbatim by three callers that would otherwise each have their own
//! near-copy: the build script, which uses it to refuse an image the libc could
//! not use; the integration test, which uses it to find the entry point it then
//! calls for real; and the kernel, for the same reason a build script needs it —
//! to check its own work. Keeping one implementation means "musl can find this
//! symbol" is a single claim verified once, not three approximations that can
//! drift apart.
//!
//! It allocates nothing, indexes nothing without bounds checking, and compiles
//! under `no_std`, because the kernel is one of those three callers. Every
//! malformed input yields `None`.

// Not in the prelude on edition 2018, which this crate shares with the rest of
// the kernel.
use core::convert::{TryFrom, TryInto};

/// Program-header and dynamic-section constants, spelled out because pulling an
/// ELF crate into the kernel for twenty numbers is a bad trade.
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_NULL: u64 = 0;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_GNU_HASH: u64 = 0x6fff_fef5;

const SYM_SIZE: usize = 24;

/// `OK_TYPES` in musl: NOTYPE(0), OBJECT(1), FUNC(2), COMMON(5).
const OK_TYPES: u32 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 5);
/// `OK_BINDS` in musl: GLOBAL(1), WEAK(2), GNU_UNIQUE(10). Bit 0 is STB_LOCAL,
/// which is deliberately excluded.
const OK_BINDS: u32 = (1 << 1) | (1 << 2) | (1 << 10);

// Every offset below comes out of the image, which is a file this code did not
// write, so `off` can be any number a 64-bit field can hold. `off + 2` is not a
// bounds check away from being safe: it overflows *before* `get` is ever asked,
// which panics in a debug build and wraps into a valid index in a release one.
// So the addition is the check, and `get` only confirms it.
fn u16le(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(off..off.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32le(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(off..off.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u64le(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(off..off.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Compares a NUL-terminated string at `off` with `want`, without allocating.
fn name_eq(b: &[u8], off: usize, want: &[u8]) -> bool {
    let end = match off.checked_add(want.len()) {
        Some(end) => end,
        None => return false,
    };
    match b.get(off..end) {
        Some(s) if s == want => b.get(end) == Some(&0),
        _ => false,
    }
}

/// Resolves `want` to its offset from the start of the image, or `None`.
///
/// The returned value is an offset rather than an address because the image has
/// not been mapped anywhere yet from this function's point of view; add the
/// mapping base to get what musl would return.
///
/// The version argument musl takes ("LINUX_2.6") has no counterpart here: musl
/// skips version checking entirely for images without a `DT_VERDEF`
/// (`if (!verdef) versym = 0;`), and the build script asserts this image has
/// none. Adding one later would silently break every lookup, which is why that
/// assertion exists.
pub fn vdsosym(img: &[u8], want: &[u8]) -> Option<u64> {
    // musl: `base = (size_t)eh + ph->p_offset - ph->p_vaddr` for each PT_LOAD,
    // keeping the last. This image is built with exactly one PT_LOAD at 0/0 so
    // the bias is zero, but compute it the same way rather than assuming — if
    // the invariant ever breaks, this follows the libc into the same answer
    // instead of quietly disagreeing with it.
    if img.get(..4)? != b"\x7fELF" {
        return None;
    }
    let phoff = u64le(img, 32)? as usize;
    let phentsize = u16le(img, 54)? as usize;
    let phnum = u16le(img, 56)? as usize;

    let mut base: i64 = 0;
    let mut dyn_off = None;
    for i in 0..phnum {
        let ph = phoff.checked_add(i.checked_mul(phentsize)?)?;
        match u32le(img, ph)? {
            PT_LOAD => {
                let p_offset = u64le(img, ph.checked_add(8)?)? as i64;
                let p_vaddr = u64le(img, ph.checked_add(16)?)? as i64;
                base = p_offset.checked_sub(p_vaddr)?;
            }
            PT_DYNAMIC => dyn_off = Some(u64le(img, ph.checked_add(8)?)? as usize),
            _ => {}
        }
    }

    // Every vaddr below is turned into a file offset with the same bias, which
    // is what makes this work on both an unmapped file image and a mapped one.
    let at =
        |vaddr: u64| -> Option<usize> { usize::try_from((vaddr as i64).checked_add(base)?).ok() };

    let mut strtab = None;
    let mut symtab = None;
    let mut hash = None;
    let mut gnu_hash = None;
    let mut d = dyn_off?;
    loop {
        let tag = u64le(img, d)?;
        let val = u64le(img, d.checked_add(8)?)?;
        match tag {
            DT_NULL => break,
            DT_STRTAB => strtab = Some(at(val)?),
            DT_SYMTAB => symtab = Some(at(val)?),
            DT_HASH => hash = Some(at(val)?),
            DT_GNU_HASH => gnu_hash = Some(at(val)?),
            _ => {}
        }
        d = d.checked_add(16)?;
    }
    let (strtab, symtab) = (strtab?, symtab?);

    // musl: `if (hashtab) nsym = hashtab[1]; else if (ghashtab) nsym =
    // count_syms_gnu(ghashtab);` — either table is enough, and this image
    // carries both.
    let nsym = match (hash, gnu_hash) {
        (Some(h), _) => u32le(img, h.checked_add(4)?)? as usize,
        (None, Some(gh)) => count_syms_gnu(img, gh)?,
        (None, None) => return None,
    };

    for i in 0..nsym {
        let sym = symtab.checked_add(i.checked_mul(SYM_SIZE)?)?;
        let info = *img.get(sym.checked_add(4)?)? as u32;
        if (1u32 << (info & 0xf)) & OK_TYPES == 0 {
            continue;
        }
        if (1u32 << (info >> 4)) & OK_BINDS == 0 {
            continue;
        }
        if u16le(img, sym.checked_add(6)?)? == 0 {
            continue; // st_shndx == SHN_UNDEF
        }
        let name = strtab.checked_add(u32le(img, sym)? as usize)?;
        if !name_eq(img, name, want) {
            continue;
        }
        return u64le(img, sym.checked_add(8)?);
    }
    None
}

/// musl's `count_syms_gnu`: a GNU hash table records no symbol count, so the
/// highest bucket's chain is walked to its terminator to recover one.
fn count_syms_gnu(img: &[u8], gh: usize) -> Option<usize> {
    let nbuckets = u32le(img, gh)? as usize;
    let symoffset = u32le(img, gh.checked_add(4)?)? as usize;
    let bloom_size = u32le(img, gh.checked_add(8)?)? as usize;
    let buckets = gh
        .checked_add(16)?
        .checked_add(bloom_size.checked_mul(8)?)?;

    let mut last = 0usize;
    for b in 0..nbuckets {
        last = last.max(u32le(img, buckets.checked_add(b.checked_mul(4)?)?)? as usize);
    }
    if last < symoffset {
        return Some(symoffset);
    }
    let chains = buckets.checked_add(nbuckets.checked_mul(4)?)?;
    let mut i = last;
    while u32le(img, chains.checked_add((i - symoffset).checked_mul(4)?)?)? & 1 == 0 {
        i = i.checked_add(1)?;
    }
    i.checked_add(1)
}

#[cfg(test)]
mod tests {
    //! Host tests for the symbol lookup, on images built here.
    //!
    //! This crate is a workspace member, but `default-members = ["xtask"]`
    //! means a bare `cargo test` never builds it, and the CI's list of crates
    //! did not name it either — so until these existed nothing in this file
    //! had ever been compiled by a test job, let alone run.
    //!
    //! The images are fixed-size arrays rather than `Vec`s so the tests need
    //! no allocator and this stays a `no_std` crate throughout.

    use super::*;

    const IMG_LEN: usize = 512;
    const PHOFF: usize = 0x40;
    const DYN: usize = 0x100;
    const HASH: usize = 0x140;
    const SYMTAB: usize = 0x160;
    const STRTAB: usize = 0x1c0;
    /// The one real symbol; index 1, because index 0 is the null entry.
    const SYM1: usize = SYMTAB + SYM_SIZE;

    /// `STB_GLOBAL << 4 | STT_FUNC`, the ordinary shape of an exported
    /// function.
    const GLOBAL_FUNC: u8 = (1 << 4) | 2;

    /// A well-formed image exporting one symbol, `tictac`, at 0x1234.
    ///
    /// One `PT_LOAD` at offset 0 vaddr 0, so the load bias is zero and file
    /// offsets and virtual addresses coincide — which is how the real image is
    /// linked, and what the tests that care about the bias then change.
    fn image() -> [u8; IMG_LEN] {
        let mut b = [0u8; IMG_LEN];
        b[..4].copy_from_slice(b"\x7fELF");
        put64(&mut b, 32, PHOFF as u64); // e_phoff
        put16(&mut b, 54, 56); // e_phentsize
        put16(&mut b, 56, 2); // e_phnum

        // PT_LOAD at 0/0.
        put32(&mut b, PHOFF, PT_LOAD);
        put64(&mut b, PHOFF + 8, 0); // p_offset
        put64(&mut b, PHOFF + 16, 0); // p_vaddr
                                      // PT_DYNAMIC.
        put32(&mut b, PHOFF + 56, PT_DYNAMIC);
        put64(&mut b, PHOFF + 56 + 8, DYN as u64);

        put64(&mut b, DYN, DT_HASH);
        put64(&mut b, DYN + 8, HASH as u64);
        put64(&mut b, DYN + 16, DT_SYMTAB);
        put64(&mut b, DYN + 24, SYMTAB as u64);
        put64(&mut b, DYN + 32, DT_STRTAB);
        put64(&mut b, DYN + 40, STRTAB as u64);
        put64(&mut b, DYN + 48, DT_NULL);

        put32(&mut b, HASH, 1); // nbucket
        put32(&mut b, HASH + 4, 2); // nchain, which is the symbol count

        // Symbol 0 is the null entry and stays zero. Symbol 1 is the real one.
        put32(&mut b, SYM1, 1); // st_name: offset into strtab
        b[SYM1 + 4] = GLOBAL_FUNC; // st_info
        put16(&mut b, SYM1 + 6, 1); // st_shndx: anything but SHN_UNDEF
        put64(&mut b, SYM1 + 8, 0x1234); // st_value

        b[STRTAB] = 0; // the empty name every strtab starts with
        b[STRTAB + 1..STRTAB + 8].copy_from_slice(b"tictac\0");
        b
    }

    fn put16(b: &mut [u8], off: usize, v: u16) {
        b[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], off: usize, v: u64) {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn a_well_formed_image_gives_up_the_symbol_it_exports() {
        assert_eq!(vdsosym(&image(), b"tictac"), Some(0x1234));
    }

    #[test]
    fn a_name_that_is_not_there_is_not_invented() {
        assert_eq!(vdsosym(&image(), b"nope"), None);
    }

    #[test]
    fn a_name_that_is_only_a_prefix_is_not_a_match() {
        // `name_eq` compares bytes and then insists on the NUL, because
        // without that check every symbol whose name starts with what was
        // asked for would answer to it.
        assert_eq!(vdsosym(&image(), b"tic"), None);
    }

    #[test]
    fn a_local_symbol_is_not_offered_to_a_libc() {
        // OK_BINDS excludes STB_LOCAL on purpose: a local symbol is not part
        // of the interface, and two objects may each have one by the same
        // name.
        let mut b = image();
        b[SYM1 + 4] = 2; // STB_LOCAL << 4 | STT_FUNC
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_weak_symbol_is_offered() {
        let mut b = image();
        b[SYM1 + 4] = (2 << 4) | 2; // STB_WEAK | STT_FUNC
        assert_eq!(vdsosym(&b, b"tictac"), Some(0x1234));
    }

    #[test]
    fn a_symbol_of_a_type_a_libc_will_not_call_is_not_a_match() {
        let mut b = image();
        b[SYM1 + 4] = (1 << 4) | 3; // STT_SECTION
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn an_undefined_symbol_is_not_a_match() {
        // st_shndx == SHN_UNDEF means the name is referenced, not defined.
        // Returning its st_value would hand the caller a zero to call.
        let mut b = image();
        put16(&mut b, SYM1 + 6, 0);
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn the_load_bias_turns_a_virtual_address_into_a_file_offset() {
        // musl computes `p_offset - p_vaddr` for each PT_LOAD and keeps the
        // last. The real image is linked at 0/0 so the bias is zero and the
        // arithmetic is invisible; here it is not.
        let mut b = image();
        put64(&mut b, PHOFF + 8, 0x1000); // p_offset
        put64(&mut b, PHOFF + 16, 0x1000); // p_vaddr, so the bias is still 0
        assert_eq!(vdsosym(&b, b"tictac"), Some(0x1234));

        // A non-zero bias: the dynamic section's own vaddrs are now 0x1000
        // above their file offsets, so the tables must be named that way.
        let mut b = image();
        put64(&mut b, PHOFF + 8, 0); // p_offset
        put64(&mut b, PHOFF + 16, 0x1000); // p_vaddr, bias = -0x1000
        put64(&mut b, DYN + 8, (HASH + 0x1000) as u64);
        put64(&mut b, DYN + 24, (SYMTAB + 0x1000) as u64);
        put64(&mut b, DYN + 40, (STRTAB + 0x1000) as u64);
        assert_eq!(vdsosym(&b, b"tictac"), Some(0x1234));
    }

    #[test]
    fn something_that_is_not_an_elf_is_refused_at_the_magic() {
        assert_eq!(vdsosym(b"", b"tictac"), None);
        assert_eq!(vdsosym(b"\x7fELG", b"tictac"), None);
        assert_eq!(vdsosym(&[0u8; 512], b"tictac"), None);
    }

    #[test]
    fn an_image_cut_short_anywhere_yields_none_and_not_a_crash() {
        // The image is a file this kernel did not write — the build script
        // reads back what the linker produced, and the kernel reads back what
        // the build script embedded. Every one of those steps can hand over
        // something shorter than it should be.
        let b = image();
        for n in 0..b.len() {
            let _ = vdsosym(&b[..n], b"tictac");
        }
    }

    #[test]
    fn a_program_header_table_that_starts_past_the_end_yields_none() {
        // `e_phoff` is a 64-bit field read straight out of the file. The
        // module's own promise is that every malformed input yields `None`:
        // an offset near the top of the address space must not be added to
        // and indexed with, which overflows before the bounds check can
        // refuse it.
        let mut b = image();
        put64(&mut b, 32, u64::MAX - 10);
        assert_eq!(vdsosym(&b, b"tictac"), None);
        put64(&mut b, 32, u64::MAX);
        assert_eq!(vdsosym(&b, b"tictac"), None);
        put64(&mut b, 32, IMG_LEN as u64);
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_load_bias_that_overflows_is_refused_and_not_wrapped() {
        // The bias is `p_offset - p_vaddr`, two 64-bit fields straight out of
        // the file, and every table address in the image is then read through
        // it. A bias at the end of the range has to refuse the addition, not
        // wrap it into an offset that looks perfectly reasonable.
        let mut b = image();
        put64(&mut b, PHOFF + 8, i64::MAX as u64); // p_offset
        put64(&mut b, PHOFF + 16, 0); // p_vaddr, so the bias is i64::MAX
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_program_header_whose_subtraction_overflows_is_refused() {
        // And the subtraction that produces the bias is itself two numbers
        // out of the file.
        let mut b = image();
        put64(&mut b, PHOFF + 8, 0); // p_offset
        put64(&mut b, PHOFF + 16, i64::MIN as u64); // p_vaddr
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_program_header_table_that_does_not_fit_yields_none() {
        let mut b = image();
        put16(&mut b, 54, u16::MAX); // e_phentsize
        put16(&mut b, 56, u16::MAX); // e_phnum
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_dynamic_section_that_starts_past_the_end_yields_none() {
        // `p_offset` is a 64-bit field out of the file like `e_phoff`, and the
        // dynamic walk reads eight bytes at a time: the last few offsets a
        // usize can hold are the ones where adding eight to look at them is
        // already too late.
        for start in [u64::MAX - 10, u64::MAX - 3, u64::MAX] {
            let mut b = image();
            put64(&mut b, PHOFF + 56 + 8, start);
            assert_eq!(vdsosym(&b, b"tictac"), None, "{:#x}", start);
        }
    }

    #[test]
    fn tables_that_point_past_the_end_yield_none() {
        for (off, name) in [
            (DYN + 8, "hash"),
            (DYN + 24, "symtab"),
            (DYN + 40, "strtab"),
        ] {
            let mut b = image();
            put64(&mut b, off, u64::MAX - 10);
            assert_eq!(vdsosym(&b, b"tictac"), None, "{}", name);
        }
    }

    #[test]
    fn a_symbol_count_larger_than_the_image_stops_at_the_image() {
        // `nchain` is the symbol count, and it is a number in the file: an
        // image can claim four billion symbols while carrying two. The walk
        // has to end at the edge of the image rather than reading past it,
        // and a name that is really there is still found on the way there.
        let mut b = image();
        put32(&mut b, HASH + 4, u32::MAX);
        assert_eq!(vdsosym(&b, b"tictac"), Some(0x1234));
        assert_eq!(vdsosym(&b, b"nope"), None);
    }

    #[test]
    fn a_name_offset_past_the_end_of_the_string_table_yields_none() {
        let mut b = image();
        put32(&mut b, SYM1, u32::MAX);
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_dynamic_section_with_no_terminator_stops_at_the_end_of_the_image() {
        // DT_NULL is what ends the walk, and it is a byte in the file like
        // any other. A dynamic section that runs to the last entry of the
        // image without one has to stop anyway, which it does because the
        // next entry falls off the end.
        let mut b = image();
        put64(&mut b, PHOFF + 56 + 8, (IMG_LEN - 16) as u64);
        put64(&mut b, IMG_LEN - 16, DT_HASH);
        put64(&mut b, IMG_LEN - 8, HASH as u64);
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_terminator_that_arrives_late_still_keeps_what_came_before_it() {
        // The other half of the same rule: entries read before the walk ends
        // count, wherever the terminator turns out to be. Here DT_NULL is one
        // entry further along than the image was built with, and the three
        // tables named before it are still the answer.
        let mut b = image();
        put64(&mut b, DYN + 48, DT_HASH);
        put64(&mut b, DYN + 56, HASH as u64);
        assert_eq!(vdsosym(&b, b"tictac"), Some(0x1234));
    }

    #[test]
    fn an_image_with_neither_hash_table_yields_none() {
        // Without DT_HASH or DT_GNU_HASH there is no symbol count, and musl
        // gives up rather than guessing one.
        let mut b = image();
        put64(&mut b, DYN, 0x99); // a tag this code does not read
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    /// The same image with a GNU hash table in place of the classic one.
    ///
    /// `count_syms_gnu` exists because a GNU hash table records no symbol
    /// count: the highest bucket's chain has to be walked to its terminator
    /// to recover one. Nothing had ever reached that code, here or in the
    /// kernel, because the real image carries both tables and `DT_HASH` wins.
    fn gnu_image() -> [u8; IMG_LEN] {
        let mut b = image();
        put64(&mut b, DYN, DT_GNU_HASH);
        put32(&mut b, HASH, 1); // nbuckets
        put32(&mut b, HASH + 4, 1); // symoffset: symbol 0 is not in the table
        put32(&mut b, HASH + 8, 1); // bloom_size, counted in 64-bit words
        put32(&mut b, HASH + 12, 0); // bloom_shift
                                     // The bloom filter itself is HASH+16..HASH+24 and stays zero.
        put32(&mut b, HASH + 24, 1); // the one bucket: last symbol index in it
        put32(&mut b, HASH + 28, 1); // its chain word, odd, which ends the chain
        b
    }

    #[test]
    fn a_gnu_hash_table_alone_is_enough_to_find_a_symbol() {
        assert_eq!(vdsosym(&gnu_image(), b"tictac"), Some(0x1234));
        assert_eq!(vdsosym(&gnu_image(), b"nope"), None);
    }

    #[test]
    fn a_gnu_table_whose_buckets_are_all_below_the_offset_counts_no_symbols() {
        // Every bucket below `symoffset` means the table hashes no symbol at
        // all, and the count is the offset itself. musl returns there rather
        // than walking a chain, because the chain array starts at `symoffset`
        // and walking from below it would be reading the buckets again.
        let mut b = gnu_image();
        put32(&mut b, HASH + 24, 0);
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_gnu_chain_with_no_terminator_stops_instead_of_running_on() {
        // A chain ends at the first odd word. Without one the walk has to
        // stop at the edge of the image, and the count it comes back with is
        // whatever it reached — which still must not be a hang or a panic.
        let mut b = gnu_image();
        put32(&mut b, HASH + 28, 0);
        assert_eq!(vdsosym(&b, b"tictac"), Some(0x1234));
        assert_eq!(vdsosym(&b, b"nope"), None);
    }

    /// A GNU table placed so its chain array is the last word of the image.
    ///
    /// The terminator is then the only thing between the walk and the end of
    /// the buffer, which is what makes "stop on the odd word" observable: get
    /// the bit wrong and the next read has nowhere to go.
    fn gnu_image_at_the_edge() -> [u8; IMG_LEN] {
        const GH: usize = IMG_LEN - 32; // so chains = GH + 16 + 8 + 4 = IMG_LEN - 4
        let mut b = image();
        put64(&mut b, DYN, DT_GNU_HASH);
        put64(&mut b, DYN + 8, GH as u64);
        put32(&mut b, GH, 1); // nbuckets
        put32(&mut b, GH + 4, 1); // symoffset
        put32(&mut b, GH + 8, 1); // bloom_size
        put32(&mut b, GH + 12, 0); // bloom_shift
        put32(&mut b, GH + 24, 1); // the one bucket
        put32(&mut b, GH + 28, 1); // its chain word: odd, and the last word there is
        b
    }

    #[test]
    fn a_gnu_chain_stops_on_its_terminator_and_not_one_word_later() {
        assert_eq!(vdsosym(&gnu_image_at_the_edge(), b"tictac"), Some(0x1234));

        // The same table with an even word where the terminator was: now the
        // walk has to read past the end of the image to find one, and says so.
        let mut b = gnu_image_at_the_edge();
        put32(&mut b, IMG_LEN - 4, 2);
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_gnu_hash_table_past_the_end_yields_none() {
        let mut b = gnu_image();
        put64(&mut b, DYN + 8, (IMG_LEN - 4) as u64);
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_bloom_filter_size_out_of_the_file_does_not_become_an_offset() {
        // `bloom_size` is counted in 64-bit words and multiplied by eight to
        // step over the filter and reach the buckets. It is a number in the
        // file like any other.
        let mut b = gnu_image();
        put32(&mut b, HASH + 8, u32::MAX);
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_gnu_image_cut_short_anywhere_yields_none_and_not_a_crash() {
        let b = gnu_image();
        for n in 0..b.len() {
            let _ = vdsosym(&b[..n], b"tictac");
        }
    }

    #[test]
    fn the_image_this_kernel_carries_exports_what_a_libc_looks_for() {
        // The build script checks the same thing, but it checks the image it
        // just linked; this checks the bytes that actually ended up in the
        // binary. Skipped when the build had no C compiler, which is the one
        // case where there is no image to ask.
        if !crate::AVAILABLE {
            return;
        }
        for name in [
            &b"__vdso_clock_gettime"[..],
            &b"__vdso_gettimeofday"[..],
            &b"__vdso_time"[..],
        ] {
            assert!(
                vdsosym(crate::IMAGE, name).is_some(),
                "{}",
                core::str::from_utf8(name).unwrap()
            );
        }
        assert_eq!(vdsosym(crate::IMAGE, b"__vdso_not_a_symbol"), None);
    }

    #[test]
    fn the_real_image_survives_being_cut_short_at_every_length() {
        if !crate::AVAILABLE {
            return;
        }
        for n in 0..crate::IMAGE.len().min(4096) {
            let _ = vdsosym(&crate::IMAGE[..n], b"__vdso_time");
        }
    }
}
