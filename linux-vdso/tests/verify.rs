//! The build script's own verifier, driven against images that break the
//! properties it checks.
//!
//! `build.rs` walks the image it just linked and refuses everything that would
//! make a vDSO silently useless: a second `PT_LOAD` so musl computes the wrong
//! load bias, a missing hash table so it finds no symbols at all, a relocation
//! nobody will ever apply. Each of those refusals is a `panic!` that had never
//! executed. The happy path runs on every build of the kernel; the guards ran
//! in no test and on no CI job, because `build.rs` is not a target any
//! `cargo test` compiles.
//!
//! Which means a guard could be wrong in either direction and nobody would
//! find out. Refusing a good image at least stops the build loudly. Accepting
//! a bad one is the failure this whole file exists to prevent, and it is
//! invisible until a guest is running -- or, worse, until a clock is quietly
//! wrong.
//!
//! The fixture is the real linked image, taken unstripped out of `OUT_DIR`,
//! and each test breaks exactly one property in it. Deliberately: a synthetic
//! image would only prove the checks work on an image nobody ships, and an
//! image good enough to reach the *last* check is most of the work anyway.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

/// The verifier itself, not a second copy of it. A copy could agree with these
/// tests and disagree with the one that actually guards the build -- the same
/// argument `build.rs` makes for including `src/elf.rs` instead of re-deriving
/// musl's lookup, one level up.
#[path = "../build.rs"]
#[allow(dead_code)]
mod build;

/// `src/elf.rs` comes along with the include above, and two of its own tests
/// read the shipped image through `crate::AVAILABLE` and `crate::IMAGE`. Inside
/// the library those resolve; here the crate root is this file, so they need a
/// home. The cost is that `elf.rs`'s synthetic-image tests run a second time in
/// this binary: microseconds, and no loss of meaning.
pub use linux_vdso::{AVAILABLE, DATA_OFFSET, DATA_SIZE, IMAGE, IMAGE_LEN};

/// The image as the linker produced it, before `strip` cuts off the section
/// headers. `verify` needs those: `_vdso_data` is in `.symtab`, and `.symtab` is
/// not part of any segment.
///
/// Empty on a host with no usable `cc`, which is why `build.rs` writes an empty
/// one rather than none: this is an `include_bytes!`, so a missing file is a
/// compile error rather than a skipped test.
const LINKED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso.so"));

// ---------------------------------------------------------------------------
// ELF64 field offsets, and the readers and writers for them
// ---------------------------------------------------------------------------

const E_SHOFF: usize = 40;
const E_PHENTSIZE: usize = 54;
const E_PHNUM: usize = 56;
const E_SHENTSIZE: usize = 58;
const E_SHNUM: usize = 60;
const E_SHSTRNDX: usize = 62;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const SHT_SYMTAB: u32 = 2;
const SHT_RELR: u32 = 19;

const DT_SONAME: u64 = 14;
const DT_HASH: u64 = 4;
const DT_GNU_HASH: u64 = 0x6fff_fef5;
const DT_VERDEF: u64 = 0x6fff_fffc;
const DT_RELRSZ: u64 = 35;
const DT_RELR: u64 = 36;

/// `p_filesz` and `p_memsz`, from the start of a program header.
const P_OFFSET: usize = 8;
const P_FILESZ: usize = 32;
const P_MEMSZ: usize = 40;

/// `sh_type`, `sh_offset`, `sh_size`, `sh_link` and `sh_entsize`, from the start
/// of a section header.
const SH_TYPE: usize = 4;
const SH_OFFSET: usize = 24;
const SH_SIZE: usize = 32;
const SH_LINK: usize = 40;
const SH_ENTSIZE: usize = 56;

/// `st_value` and `st_size`, from the start of a symbol-table entry.
const ST_VALUE: usize = 8;
const ST_SIZE: usize = 16;

fn u16at(b: &[u8], off: usize) -> usize {
    u16::from_le_bytes([b[off], b[off + 1]]) as usize
}

fn u32at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn u64at(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(v)
}

fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn name_at(b: &[u8], off: usize) -> &str {
    let end = off + b[off..].iter().position(|&c| c == 0).unwrap_or(0);
    core::str::from_utf8(&b[off..end]).unwrap_or("")
}

// ---------------------------------------------------------------------------
// Finding the thing to break
// ---------------------------------------------------------------------------

/// Byte offset of the first program header of type `p_type`.
///
/// Found rather than hardcoded: an offset written down here would go stale the
/// first time the linker script moves something, and go stale *silently*,
/// because breaking a byte that is no longer the field you meant still makes
/// `verify` refuse the image -- for the wrong reason, in a test that still
/// passes.
fn phdr(b: &[u8], p_type: u32) -> usize {
    let phoff = u64at(b, 32) as usize;
    let entsize = u16at(b, E_PHENTSIZE);
    (0..u16at(b, E_PHNUM))
        .map(|i| phoff + i * entsize)
        .find(|&ph| u32at(b, ph) == p_type)
        .unwrap_or_else(|| panic!("la imagen no tiene un segmento de tipo {}", p_type))
}

/// Byte offset of the section header named `want`.
fn shdr(b: &[u8], want: &str) -> usize {
    let shoff = u64at(b, E_SHOFF) as usize;
    let entsize = u16at(b, E_SHENTSIZE);
    let shstr = u64at(b, shoff + u16at(b, E_SHSTRNDX) * entsize + SH_OFFSET) as usize;
    (0..u16at(b, E_SHNUM))
        .map(|i| shoff + i * entsize)
        .find(|&sh| name_at(b, shstr + u32at(b, sh) as usize) == want)
        .unwrap_or_else(|| panic!("la imagen no tiene una seccion {:?}", want))
}

/// Byte offset of section `index`'s header.
fn shdr_at(b: &[u8], index: usize) -> usize {
    u64at(b, E_SHOFF) as usize + index * u16at(b, E_SHENTSIZE)
}

/// Byte offset of `.symtab`'s entry for the symbol named `want`, and of that
/// name inside the string table `.symtab` points at.
fn sym(b: &[u8], want: &str) -> (usize, usize) {
    let symtab = shdr(b, ".symtab");
    assert_eq!(u32at(b, symtab + SH_TYPE), SHT_SYMTAB);
    let strtab = shdr_at(b, u32at(b, symtab + SH_LINK) as usize);
    let symstr = u64at(b, strtab + SH_OFFSET) as usize;
    let off = u64at(b, symtab + SH_OFFSET) as usize;
    let entsize = u64at(b, symtab + SH_ENTSIZE) as usize;
    for i in 0..(u64at(b, symtab + SH_SIZE) as usize / entsize) {
        let s = off + i * entsize;
        let name = symstr + u32at(b, s) as usize;
        if name_at(b, name) == want {
            return (s, name);
        }
    }
    panic!("la imagen no exporta {:?}", want);
}

/// Byte offset of the 16-byte `.dynamic` entry carrying `tag`.
fn dyn_slot(b: &[u8], tag: u64) -> usize {
    let dynamic = phdr(b, PT_DYNAMIC);
    let off = u64at(b, dynamic + P_OFFSET) as usize;
    let size = u64at(b, dynamic + P_FILESZ) as usize;
    let mut i = 0;
    while i + 16 <= size {
        if u64at(b, off + i) == tag {
            return off + i;
        }
        i += 16;
    }
    panic!("la imagen no tiene la etiqueta dinamica {:#x}", tag);
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// The linked image, or `None` when this build has none to break.
fn fixture() -> Option<Vec<u8>> {
    if !AVAILABLE || LINKED.is_empty() {
        return None;
    }
    Some(LINKED.to_vec())
}

/// Break one property and assert `verify` refuses, *saying which one*.
///
/// `catch_unwind` rather than `#[should_panic]`, for two reasons. The message
/// has to be matched against the property under test: a test that passes
/// because some unrelated assert fired first is worse than no test, and while
/// building these that happened more than once. And a host with no `cc` has no
/// image to break, where a `#[should_panic]` test would fail instead of
/// stepping aside.
#[track_caller]
fn refuses(expected: &str, break_it: impl FnOnce(&mut Vec<u8>)) {
    let Some(mut img) = fixture() else { return };
    break_it(&mut img);
    let err = std::panic::catch_unwind(|| build::verify(&img))
        .err()
        .unwrap_or_else(|| panic!("verify acepto una imagen que rompe {:?}", expected));
    let msg = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&str>().copied())
        .unwrap_or("<panico sin mensaje>")
        .to_string();
    assert!(
        msg.contains(expected),
        "verify rechazo la imagen, pero por otra cosa.\n  se esperaba: {:?}\n  dijo:        {:?}",
        expected,
        msg
    );
}

/// Change something `verify` does not check, and assert it still accepts.
///
/// The other half of every refusal: a check that fires for the wrong reason is
/// a check that will fire on a good image one day.
#[track_caller]
fn still_accepts(change_it: impl FnOnce(&mut Vec<u8>)) {
    let Some(mut img) = fixture() else { return };
    change_it(&mut img);
    build::verify(&img);
}

// ---------------------------------------------------------------------------
// The image this build actually linked
// ---------------------------------------------------------------------------

#[test]
fn the_image_this_build_linked_passes_its_own_verification() {
    let Some(img) = fixture() else { return };
    let v = build::verify(&img);
    assert_eq!(v.data_offset % 4096, 0, "_vdso_data no esta en una pagina");
    assert!(v.load_len <= img.len(), "el PT_LOAD no cabe en el fichero");
    assert!(v.data_offset + v.data_size <= v.load_len);
}

/// The invariant the crate exists for, stated where a reader will see it.
///
/// `DATA_SIZE` is `_vdso_data`'s `st_size` in the linked image, i.e. the size
/// the C compiler gave `struct vdso_data` in `vdso.c`. `VdsoData` is the Rust
/// view the kernel writes through. They are two spellings of one layout, and
/// before this they were only promised to match in a doc comment.
#[test]
fn the_c_struct_and_the_rust_view_are_the_same_size() {
    if !AVAILABLE {
        return;
    }
    assert_eq!(
        DATA_SIZE,
        core::mem::size_of::<linux_vdso::VdsoData>(),
        "vdso.c y VdsoData han dejado de describir la misma cosa"
    );
    assert!(DATA_OFFSET + DATA_SIZE <= IMAGE_LEN);
    assert_eq!(IMAGE.len(), IMAGE_LEN);
}

// ---------------------------------------------------------------------------
// Relocations: the hole that was there
// ---------------------------------------------------------------------------

/// `DT_RELR` is the packed encoding of relative relocations, and a linker told
/// to use it emits it *instead of* `DT_RELA` -- so a check that looks only for
/// RELA and REL sees an image with no relocations when it is full of them.
/// Nothing in this kernel applies them, which is the failure the module comment
/// of `build.rs` opens with.
#[test]
fn a_packed_relocation_table_is_refused_like_an_unpacked_one() {
    refuses("DT_RELR presente", |img| {
        let slot = dyn_slot(img, DT_SONAME);
        put64(img, slot, DT_RELR);
        put64(img, slot + 8, 0x200);
    });
}

#[test]
fn the_size_of_a_packed_relocation_table_is_refused_too() {
    refuses("DT_RELRSZ presente", |img| {
        let slot = dyn_slot(img, DT_SONAME);
        put64(img, slot, DT_RELRSZ);
        put64(img, slot + 8, 8);
    });
}

/// The dynamic table is what a loader reads, but the section table is what says
/// whether the bytes are there at all -- and `-fvisibility=hidden` is what is
/// supposed to keep them from existing. Both halves have to see RELR.
#[test]
fn a_packed_relocation_section_with_bytes_in_it_is_refused() {
    refuses("seccion de reubicaciones", |img| {
        let text = shdr(img, ".text");
        assert_ne!(u64at(img, text + SH_SIZE), 0, ".text esta vacia");
        put32(img, text + SH_TYPE, SHT_RELR);
    });
}

/// An empty relocation section is a section header and nothing else. The
/// distinction matters: refusing it would fail builds over a linker's
/// bookkeeping.
#[test]
fn an_empty_relocation_section_is_not_a_problem() {
    still_accepts(|img| {
        let null = shdr_at(img, 0);
        assert_eq!(u64at(img, null + SH_SIZE), 0);
        put32(img, null + SH_TYPE, SHT_RELR);
    });
}

// ---------------------------------------------------------------------------
// `_vdso_data`: where the clock lands
// ---------------------------------------------------------------------------

/// The bound is `data_offset + data_size`, and `data_size` comes from the
/// symbol table rather than being written out here, so growing the C struct
/// past the end of the image is caught. It used to be a hardcoded three
/// `u64`s -- which happened to equal the struct today, and would have gone on
/// agreeing with nothing the moment a field was added.
#[test]
fn a_struct_that_does_not_fit_in_the_image_is_refused() {
    refuses("no cabe en la imagen", |img| {
        let (s, _) = sym(img, "_vdso_data");
        put64(img, s + ST_SIZE, 0x1_0000);
    });
}

#[test]
fn a_struct_whose_symbol_declares_no_size_is_refused() {
    refuses("st_size = 0", |img| {
        let (s, _) = sym(img, "_vdso_data");
        put64(img, s + ST_SIZE, 0);
    });
}

/// The kernel maps whole pages and hands that page out as the clock mirror, so
/// the struct has to start on one.
#[test]
fn a_struct_that_does_not_start_on_a_page_is_refused() {
    refuses("no esta alineado a pagina", |img| {
        let (s, _) = sym(img, "_vdso_data");
        let v = u64at(img, s + ST_VALUE);
        put64(img, s + ST_VALUE, v + 1);
    });
}

/// Nothing may share the clock's page, because the whole page is what gets
/// mapped. Moving `_vdso_data` to the front of the image leaves the code
/// sitting in it.
#[test]
fn an_image_that_runs_on_past_the_clock_page_is_refused() {
    refuses("mas alla de la pagina de _vdso_data", |img| {
        let (s, _) = sym(img, "_vdso_data");
        put64(img, s + ST_VALUE, 0);
    });
}

/// Without the symbol the kernel has no address to publish through, and the
/// message has to say so: a stripped image is the likely cause and it is not
/// otherwise obvious.
#[test]
fn an_image_with_no_vdso_data_symbol_is_refused() {
    refuses("no se encuentra el simbolo _vdso_data", |img| {
        let (_, name) = sym(img, "_vdso_data");
        img[name] = b'x';
    });
}

// ---------------------------------------------------------------------------
// The shape musl relies on
// ---------------------------------------------------------------------------

#[test]
fn a_second_loadable_segment_is_refused() {
    refuses("exactamente un PT_LOAD", |img| {
        let dynamic = phdr(img, PT_DYNAMIC);
        put32(img, dynamic, PT_LOAD);
    });
}

#[test]
fn a_segment_that_does_not_start_at_the_top_of_the_file_is_refused() {
    refuses("p_offset debe ser 0", |img| {
        let load = phdr(img, PT_LOAD);
        put64(img, load + P_OFFSET, 0x1000);
    });
}

#[test]
fn a_segment_with_a_bss_is_refused() {
    refuses("p_memsz != p_filesz", |img| {
        let load = phdr(img, PT_LOAD);
        let memsz = u64at(img, load + P_MEMSZ);
        put64(img, load + P_MEMSZ, memsz + 4096);
    });
}

/// `strip` cuts the file to exactly `p_filesz`, and everything `verify` reads
/// after this point is indexed by numbers out of the image. A `p_filesz` past
/// the end of what was linked used to reach both as a slice panic, which says
/// nothing about the image.
#[test]
fn a_segment_longer_than_the_file_is_refused_by_name() {
    refuses("pasa del final del fichero", |img| {
        let load = phdr(img, PT_LOAD);
        let over = (img.len() + 4096) as u64;
        put64(img, load + P_FILESZ, over);
        put64(img, load + P_MEMSZ, over);
    });
}

#[test]
fn an_image_with_neither_hash_table_is_refused() {
    refuses("falta DT_HASH y DT_GNU_HASH", |img| {
        for tag in [DT_HASH, DT_GNU_HASH] {
            let slot = dyn_slot(img, tag);
            put64(img, slot, DT_SONAME);
        }
    });
}

/// musl skips version checking entirely when there is no `DT_VERDEF`, which is
/// the only reason this image needs no version script. A `DT_VERDEF` appearing
/// turns every lookup into a failed comparison against "LINUX_2.6".
#[test]
fn a_version_definition_table_is_refused() {
    refuses("DT_VERDEF presente", |img| {
        let slot = dyn_slot(img, DT_SONAME);
        put64(img, slot, DT_VERDEF);
        put64(img, slot + 8, 0x100);
    });
}

/// A symbol table that does not say how wide its entries are cannot be walked.
/// The division by it used to stop the build with "attempt to divide by zero",
/// which names neither the file nor the field.
#[test]
fn a_symbol_table_with_no_entry_size_is_refused_by_name() {
    refuses("sh_entsize = 0", |img| {
        let symtab = shdr(img, ".symtab");
        put64(img, symtab + SH_ENTSIZE, 0);
    });
}

// ---------------------------------------------------------------------------
// `strip`
// ---------------------------------------------------------------------------

/// What the kernel maps is `strip`'s output, so the clock's page has to survive
/// it whole, and the section-header fields have to be zeroed rather than left
/// pointing past the new end of the file.
#[test]
fn stripping_keeps_the_clock_page_and_leaves_a_readable_elf() {
    let Some(img) = fixture() else { return };
    let v = build::verify(&img);
    let out = build::strip(&img, v.load_len);

    assert_eq!(out.len(), v.load_len);
    assert_eq!(&out[..4], b"\x7fELF");
    assert_eq!(u64at(&out, E_SHOFF), 0, "e_shoff sigue apuntando a nada");
    assert_eq!(u16at(&out, E_SHENTSIZE), 0);
    assert_eq!(u16at(&out, E_SHNUM), 0);
    assert_eq!(u16at(&out, E_SHSTRNDX), 0);
    assert!(
        v.data_offset + v.data_size <= out.len(),
        "el recorte se ha llevado parte de _vdso_data"
    );
    assert_eq!(out.len(), IMAGE.len(), "no es lo que se acabo enviando");
}

/// The generated constants are what the kernel compiles against, so the
/// spelling of them is load-bearing in a way a format string is not usually
/// allowed to be.
#[test]
fn the_generated_constants_say_what_the_kernel_reads() {
    let v = build::Verified {
        data_offset: 4096,
        data_size: 24,
        load_len: 4120,
    };
    let src = build::meta_source(Some(&v), 4120);
    for line in [
        "pub const AVAILABLE: bool = true;",
        "pub const DATA_OFFSET: usize = 4096;",
        "pub const DATA_SIZE: usize = 24;",
        "pub const IMAGE_LEN: usize = 4120;",
    ] {
        assert!(src.contains(line), "falta {:?} en:\n{}", line, src);
    }
}

/// There is one way to say "no image", and it carries no numbers to disagree
/// with. The arm that writes it runs only on a host with no usable `cc`, which
/// is precisely why it cannot be allowed to take an `available` flag of its own.
#[test]
fn saying_there_is_no_image_cannot_also_claim_one() {
    let none = build::meta_source(None, 4120);
    assert!(none.contains("pub const AVAILABLE: bool = false;"));
    assert!(none.contains("pub const DATA_SIZE: usize = 0;"));
    assert!(none.contains("pub const DATA_OFFSET: usize = 0;"));
    assert!(
        none.contains("pub const IMAGE_LEN: usize = 0;"),
        "un largo sin imagen: {}",
        none
    );
}
