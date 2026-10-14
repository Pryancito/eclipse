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
const DT_VERSYM: u64 = 0x6fff_fff0;
const DT_VERDEF: u64 = 0x6fff_fffc;

const SYM_SIZE: usize = 24;
/// `Elf64_Verdef`: vd_version, vd_flags, vd_ndx, vd_cnt, vd_hash, vd_aux,
/// vd_next.
const VERDEF_FLAGS: usize = 2;
const VERDEF_NDX: usize = 4;
const VERDEF_AUX: usize = 12;
const VERDEF_NEXT: usize = 16;
/// `VER_FLG_BASE`: the definition of the object itself (its soname), which is
/// never the version of a symbol.
const VER_FLG_BASE: u16 = 1;

/// The symbol version this image defines, and the one every caller asks for:
/// musl passes it literally (`__vdsosym("LINUX_2.6", ...)`) and the Go runtime
/// looks it up by hash in the `DT_VERDEF` chain.
pub const VERNAME: &[u8] = b"LINUX_2.6";

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
/// Looks the symbol up under [`VERNAME`], the version the real musl asks for.
pub fn vdsosym(img: &[u8], want: &[u8]) -> Option<u64> {
    vdsosym_ver(img, VERNAME, want)
}

/// As [`vdsosym`], with the version name spelled out.
///
/// musl takes it as its first argument and compares it against the `DT_VERDEF`
/// entry named by the symbol's `DT_VERSYM` index — unless the image defines no
/// versions at all, in which case it drops the check (`if (!verdef) versym =
/// 0;`). Both halves are implemented here, because this image used to rely on
/// that shortcut and no longer does: it defines `LINUX_2.6` so that the Go
/// runtime, which has no such shortcut, can parse it at all.
pub fn vdsosym_ver(img: &[u8], vername: &[u8], want: &[u8]) -> Option<u64> {
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
    let mut versym = None;
    let mut verdef = None;
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
            DT_VERSYM => versym = Some(at(val)?),
            DT_VERDEF => verdef = Some(at(val)?),
            _ => {}
        }
        d = d.checked_add(16)?;
    }
    let (strtab, symtab) = (strtab?, symtab?);
    // musl: `if (!verdef) versym = 0;`.
    let verdef = match verdef {
        Some(v) => v,
        None => {
            versym = None;
            0
        }
    };

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
        if let Some(vs) = versym {
            let vsym = u16le(img, vs.checked_add(i.checked_mul(2)?)?)?;
            if !checkver(img, verdef, vsym, vername, strtab) {
                continue;
            }
        }
        return u64le(img, sym.checked_add(8)?);
    }
    None
}

/// musl's `checkver`: walks the `DT_VERDEF` chain for the definition whose
/// index is the symbol's `DT_VERSYM` entry, and compares its name.
///
/// A symbol whose version is not in the chain does not match, which is what
/// musl's "run off the end of the list" arm amounts to.
fn checkver(img: &[u8], verdef: usize, vsym: u16, vername: &[u8], strtab: usize) -> bool {
    // `vd_next` is non-zero on every link but the last, so `def` strictly
    // increases and the walk runs off the end of the image — where `u16le`
    // answers `None` — rather than around a cycle a hand-written image could
    // otherwise spin in forever.
    let walk = || -> Option<bool> {
        let vsym = vsym & 0x7fff;
        let mut def = verdef;
        loop {
            let flags = u16le(img, def.checked_add(VERDEF_FLAGS)?)?;
            let ndx = u16le(img, def.checked_add(VERDEF_NDX)?)?;
            if flags & VER_FLG_BASE == 0 && (ndx & 0x7fff) == vsym {
                // `Elf64_Verdaux.vda_name`, an offset into the string table.
                let aux = def.checked_add(u32le(img, def.checked_add(VERDEF_AUX)?)? as usize)?;
                let name = strtab.checked_add(u32le(img, aux)? as usize)?;
                return Some(name_eq(img, name, vername));
            }
            match u32le(img, def.checked_add(VERDEF_NEXT)?)? {
                0 => return Some(false),
                next => def = def.checked_add(next as usize)?,
            }
        }
    };
    walk().unwrap_or(false)
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

    const IMG_LEN: usize = 1024;
    const PHOFF: usize = 0x40;
    const DYN: usize = 0x100;
    const HASH: usize = 0x180;
    const SYMTAB: usize = 0x1a0;
    const STRTAB: usize = 0x200;
    const VERSYM: usize = 0x240;
    const VERDEF: usize = 0x260;
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
        b[STRTAB + 8..STRTAB + 18].copy_from_slice(b"LINUX_2.6\0");
        b[STRTAB + 18..STRTAB + 24].copy_from_slice(b"libby\0");
        b
    }

    /// Offset of `"LINUX_2.6"` and of `"libby"` inside the string table, as the
    /// `vda_name` of a version definition records them.
    const VERNAME_OFF: u32 = 8;
    const BASENAME_OFF: u32 = 18;

    /// The same image, now carrying the version tables the real one has:
    /// a `DT_VERDEF` chain of the base definition (index 1, `VER_FLG_BASE`)
    /// followed by `LINUX_2.6` (index 2), and a `DT_VERSYM` that tags the one
    /// real symbol with index 2.
    ///
    /// This is the shape a linker produces from a `VERSION { LINUX_2.6 {...} }`
    /// script, which is what `vdso.lds` now has — so these tests cover the
    /// arm of the lookup the shipped image actually takes.
    fn versioned_image() -> [u8; IMG_LEN] {
        let mut b = image();

        // Two more dynamic tags; DT_NULL moves along to make room.
        put64(&mut b, DYN + 48, DT_VERSYM);
        put64(&mut b, DYN + 56, VERSYM as u64);
        put64(&mut b, DYN + 64, DT_VERDEF);
        put64(&mut b, DYN + 72, VERDEF as u64);
        put64(&mut b, DYN + 80, DT_NULL);

        put16(&mut b, VERSYM, 0); // symbol 0, the null entry
        put16(&mut b, VERSYM + 2, 2); // symbol 1 is a LINUX_2.6 symbol

        verdef(&mut b, VERDEF, VER_FLG_BASE, 1, BASENAME_OFF, 28);
        verdef(&mut b, VERDEF + 28, 0, 2, VERNAME_OFF, 0);
        b
    }

    /// Writes one `Elf64_Verdef` plus the single `Elf64_Verdaux` that names it,
    /// laid out the way a linker lays them out: the aux record follows its
    /// definition, and `vd_next` is the distance to the next definition.
    fn verdef(b: &mut [u8], at: usize, flags: u16, ndx: u16, name: u32, next: u32) {
        put16(b, at, 1); // vd_version
        put16(b, at + VERDEF_FLAGS, flags);
        put16(b, at + VERDEF_NDX, ndx);
        put16(b, at + 6, 1); // vd_cnt: one aux record
        put32(b, at + 8, 0); // vd_hash, which musl does not read
        put32(b, at + VERDEF_AUX, 20);
        put32(b, at + VERDEF_NEXT, next);
        put32(b, at + 20, name); // vda_name
        put32(b, at + 24, 0); // vda_next
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
    fn a_versioned_image_gives_up_the_symbol_under_the_version_it_defines() {
        assert_eq!(vdsosym(&versioned_image(), b"tictac"), Some(0x1234));
    }

    #[test]
    fn a_versioned_image_withholds_the_symbol_from_another_version() {
        // What musl does with an image whose versions it does not recognise:
        // resolve nothing, and take the syscall.
        assert_eq!(
            vdsosym_ver(&versioned_image(), b"LINUX_9.9", b"tictac"),
            None
        );
    }

    #[test]
    fn a_symbol_tagged_with_the_base_definition_is_not_a_match() {
        // Index 1 is the object's own name, flagged VER_FLG_BASE, which musl
        // skips while walking the chain — so a symbol claiming it matches
        // nothing at all rather than matching the soname.
        let mut b = versioned_image();
        put16(&mut b, VERSYM + 2, 1);
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    #[test]
    fn a_symbol_tagged_with_an_index_no_definition_carries_is_not_a_match() {
        let mut b = versioned_image();
        put16(&mut b, VERSYM + 2, 7);
        assert_eq!(vdsosym(&b, b"tictac"), None);
    }

    /// musl's `if (!verdef) versym = 0;`: with nothing to compare against, the
    /// comparison is dropped rather than failed. This is the arm the image took
    /// before it defined a version, and the one that left the Go runtime
    /// dereferencing a null `info.verdef`.
    #[test]
    fn without_a_definition_table_the_version_tags_are_ignored() {
        let mut b = versioned_image();
        put64(&mut b, DYN + 64, DT_NULL); // drop DT_VERDEF, keep DT_VERSYM
        assert_eq!(vdsosym_ver(&b, b"LINUX_9.9", b"tictac"), Some(0x1234));
    }

    /// A chain whose `vd_next` points back at itself must end the walk, not
    /// spin in it. Nothing a linker emits looks like this; an image handed to
    /// the kernel from anywhere else might.
    #[test]
    fn a_definition_chain_that_loops_does_not_hang() {
        let mut b = versioned_image();
        put16(&mut b, VERSYM + 2, 9); // an index the chain does not carry
        put32(&mut b, VERDEF + 28 + VERDEF_NEXT, 0); // ... and the chain ends
        assert_eq!(vdsosym(&b, b"tictac"), None);
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
            &b"__vdso_getcpu"[..],
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
