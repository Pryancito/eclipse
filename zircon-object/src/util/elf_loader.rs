//! ELF loading of Zircon and Linux.
use crate::{error::*, vm::*};
use alloc::sync::Arc;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
use alloc::vec;
use core::convert::TryFrom;
#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
use xmas_elf::program::ProgramHeader;
use xmas_elf::{
    header::Class,
    program::{Flags, SegmentData, Type},
    sections::{SectionData, SectionHeader, ShType, SHN_LORESERVE},
    symbol_table::{DynEntry64, Entry},
    ElfFile,
};

/// Magic bytes every ELF file starts with.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
/// `EI_CLASS` for a 32-bit image.
const ELF_CLASS32: u8 = 1;
/// `EI_CLASS` for a 64-bit image.
const ELF_CLASS64: u8 = 2;
/// `EI_DATA` for two's complement little-endian.
const ELF_DATA_LSB: u8 = 1;
/// `SHT_NOBITS`: a section that occupies no space in the file (`.bss`), so its
/// `sh_offset` says nothing about where its bytes are.
const SHT_NOBITS: u64 = 8;

/// Read a little-endian integer of `width` bytes at `at`.
///
/// The caller has already established that `at + width` is inside `data`.
fn read_le(data: &[u8], at: usize, width: usize) -> u64 {
    let mut value = 0u64;
    for i in (0..width).rev() {
        value = (value << 8) | data[at + i] as u64;
    }
    value
}

/// Where each class puts the fields this check reads, and how big one table
/// entry is. The ELF header is the one structure whose layout has to be known
/// by hand here: the parser cannot be asked where its tables are until it has
/// already sliced them out.
struct HeaderLayout {
    /// Size of the ELF header, which `parse_header` slices whole.
    header_size: usize,
    /// Width of an offset field (`e_phoff`, `p_offset`, ...).
    offset_width: usize,
    /// Offset of `e_phoff`; `e_shoff` follows one offset-width later.
    ph_offset_at: usize,
    /// Offset of `e_phentsize`; `e_phnum`, `e_shentsize` and `e_shnum` are the
    /// three `u16` that follow it.
    ph_entry_size_at: usize,
    /// Size of one program header entry.
    ph_entry_size: u64,
    /// Size of one section header entry.
    sh_entry_size: u64,
    /// Alignment every header struct of this class needs.
    ///
    /// `xmas_elf` reads each one with `zero::read`, which casts the file's own
    /// bytes to a `#[repr(C)]` struct and builds a `&T` from the result. Every
    /// field of those structs is at most one offset wide, so the alignment the
    /// reference needs is the offset width.
    entry_align: usize,
    /// Offsets of `p_offset` and `p_filesz` inside a program header.
    segment_fields: (usize, usize),
    /// Offsets of `sh_type`, `sh_offset` and `sh_size` inside a section header.
    section_fields: (usize, usize, usize),
}

impl HeaderLayout {
    /// The layout `EI_CLASS` names, or `None` when it names no class this
    /// parser knows.
    fn of_class(class: u8) -> Option<Self> {
        match class {
            ELF_CLASS32 => Some(Self {
                header_size: 52,
                offset_width: 4,
                ph_offset_at: 28,
                ph_entry_size_at: 42,
                ph_entry_size: 32,
                sh_entry_size: 40,
                entry_align: 4,
                segment_fields: (4, 16),
                section_fields: (4, 16, 20),
            }),
            ELF_CLASS64 => Some(Self {
                header_size: 64,
                offset_width: 8,
                ph_offset_at: 32,
                ph_entry_size_at: 54,
                ph_entry_size: 56,
                sh_entry_size: 64,
                entry_align: 8,
                segment_fields: (8, 32),
                section_fields: (4, 24, 32),
            }),
            _ => None,
        }
    }
}

/// Check that every byte range `xmas_elf` will slice out of `data` lies inside
/// it, and reject the file when any does not.
///
/// `xmas_elf::ElfFile::new` looks at the first 16 bytes and then trusts the
/// rest. `parse_header` goes on to slice `data[16..64]` for a 64-bit image
/// without consulting the length again; `parse_program_header` and
/// `parse_section_header` index the file at `e_phoff` and `e_shoff` with no
/// bounds check at all; and `ProgramHeader::raw_data`, which every `get_data`
/// goes through, slices `p_offset .. p_offset + p_filesz`. Each of those is a
/// plain slice index, so a malformed file panics the parser -- and `execve`
/// hands it whatever bytes the calling process named, which makes a kernel
/// panic something any unprivileged program can ask for with a 20-byte file.
///
/// Linux answers the same files with `ENOEXEC`: `fs/binfmt_elf.c` checks
/// `e_phentsize`, `e_phnum` and the extent of the program header table before
/// reading a single entry. So does this. Call it before `ElfFile::new` on any
/// data that came from userspace.
///
/// Fitting inside the file is not the whole of it. Every header the parser
/// hands back is a `#[repr(C)]` struct mapped straight onto the file's bytes,
/// so each one also has to START at an address that struct can live at --
/// `zero::read` builds the reference without checking, and a misaligned `&T`
/// is undefined behaviour whatever the machine makes of the loads. Linux never
/// meets this: it copies the tables into a buffer of its own
/// (`elf_read`/`kmalloc`) instead of casting in place, so the requirement is
/// this loader's own. It is the check [`section_data`] already makes for a
/// section's CONTENTS, asked of the tables the parser slices first.
pub fn check_elf_bounds(data: &[u8]) -> ZxResult {
    // Not an ELF at all: the same error `ElfFile::new` would have given, so a
    // shell script or a JPEG still fails the way it always did.
    if data.len() < ELF_MAGIC.len() || data[..ELF_MAGIC.len()] != ELF_MAGIC {
        return Err(ZxError::INVALID_ARGS);
    }
    if data.len() <= ELF_DATA_AT {
        return Err(ZxError::INVALID_ARGS);
    }
    // The parser maps its header structs straight onto the bytes, so it reads
    // every field in the host's byte order. A big-endian image would be read
    // as garbage, lengths included.
    if data[ELF_DATA_AT] != ELF_DATA_LSB {
        return Err(ZxError::INVALID_ARGS);
    }
    let layout = HeaderLayout::of_class(data[ELF_CLASS_AT]).ok_or(ZxError::INVALID_ARGS)?;
    if data.len() < layout.header_size {
        return Err(ZxError::INVALID_ARGS);
    }
    let file_len = data.len() as u64;

    // Where the image sits decides every address below, and `parse_header`
    // reads the header's own second half as a `HeaderPt2_<P>` at `base + 16`
    // -- aligned exactly when the image is, since 16 is a multiple of both
    // widths. Nothing in this function's signature promises an aligned slice,
    // and a `Vec<u8>` asks its allocator for an alignment of one.
    let base = data.as_ptr() as usize;
    check_aligned(base, layout.entry_align)?;

    // The two tables themselves, then every entry of each.
    let ph_table = TableExtent::read(data, &layout, 0)?;
    let sh_table = TableExtent::read(data, &layout, 1)?;
    check_table_fits(&ph_table, layout.ph_entry_size, file_len)?;
    check_table_fits(&sh_table, layout.sh_entry_size, file_len)?;

    let (p_offset_at, p_filesz_at) = layout.segment_fields;
    for entry in ph_table.entries() {
        // Per entry rather than per table: the stride is `e_phentsize`, a
        // number the file chooses, so a table at an aligned offset with an
        // odd stride leaves entry 0 where it belongs and every entry after
        // it one byte out.
        check_aligned(base.wrapping_add(entry), layout.entry_align)?;
        let offset = read_le(data, entry + p_offset_at, layout.offset_width);
        let file_size = read_le(data, entry + p_filesz_at, layout.offset_width);
        check_range_fits(offset, file_size, file_len)?;
    }

    // `e_shstrndx` is not checked here. It is an index rather than an extent,
    // and the only file it could be measured against a table for is one that
    // has a table -- which is not the file that panicked. See
    // [`section_count`], which is where every section index is bounded now.

    let (sh_type_at, sh_offset_at, sh_size_at) = layout.section_fields;
    for entry in sh_table.entries() {
        check_aligned(base.wrapping_add(entry), layout.entry_align)?;
        let offset = read_le(data, entry + sh_offset_at, layout.offset_width);
        // `get_shstr_table` slices `input[sh_offset..]` whatever the section
        // type says, so the offset has to be inside the file even for a
        // section that stores no bytes.
        check_range_fits(offset, 0, file_len)?;
        // `.bss` and friends are declared in the file but not stored in it, so
        // their size says nothing about it.
        if read_le(data, entry + sh_type_at, 4) == SHT_NOBITS {
            continue;
        }
        let size = read_le(data, entry + sh_size_at, layout.offset_width);
        check_range_fits(offset, size, file_len)?;
    }
    Ok(())
}

/// Parse `data` into an [`ElfFile`], having first checked that every range the
/// parser will slice out of it exists.
///
/// This is the constructor to use for any image that came from userspace.
/// `ElfFile::new` on its own reads sixteen bytes and trusts the rest, so
/// calling it directly and remembering to bound the data first is a thing to
/// get wrong once; there is nothing to forget here.
pub fn parse_checked_elf(data: &[u8]) -> ZxResult<ElfFile<'_>> {
    check_elf_bounds(data)?;
    ElfFile::new(data).map_err(|e| {
        warn!("elf: not a valid ELF image: {}", e);
        ZxError::INVALID_ARGS
    })
}

/// Byte offset of `EI_CLASS` in the ELF identification bytes.
const ELF_CLASS_AT: usize = 4;
/// Byte offset of `EI_DATA` in the ELF identification bytes.
const ELF_DATA_AT: usize = 5;

/// One of the two header tables, as the ELF header describes it.
struct TableExtent {
    /// `e_phoff` / `e_shoff`.
    offset: u64,
    /// `e_phentsize` / `e_shentsize`.
    entry_size: u64,
    /// `e_phnum` / `e_shnum`.
    count: u64,
}

impl TableExtent {
    /// Read the table's three header fields. `which` is 0 for the program
    /// header table and 1 for the section header table, which sit next to each
    /// other in both classes.
    fn read(data: &[u8], layout: &HeaderLayout, which: usize) -> ZxResult<Self> {
        let offset_at = layout.ph_offset_at + which * layout.offset_width;
        let entry_size_at = layout.ph_entry_size_at + which * 4;
        Ok(Self {
            offset: read_le(data, offset_at, layout.offset_width),
            entry_size: read_le(data, entry_size_at, 2),
            count: read_le(data, entry_size_at + 2, 2),
        })
    }

    /// Byte offset of each entry, once the table is known to fit.
    fn entries(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.count).map(move |i| (self.offset + i * self.entry_size) as usize)
    }
}

/// Check that a header table fits in the file and that its entries are at
/// least as large as the struct the parser will read out of each one.
fn check_table_fits(table: &TableExtent, min_entry_size: u64, file_len: u64) -> ZxResult {
    if table.count == 0 {
        // No table; the iterator yields nothing and nothing is sliced.
        return Ok(());
    }
    if table.entry_size < min_entry_size {
        return Err(ZxError::INVALID_ARGS);
    }
    check_range_fits(table.offset, table.count * table.entry_size, file_len)
}

/// Check that a `#[repr(C)]` header struct may be built at `at`.
fn check_aligned(at: usize, align: usize) -> ZxResult {
    if at.is_multiple_of(align) {
        Ok(())
    } else {
        Err(ZxError::INVALID_ARGS)
    }
}

/// Check that `offset .. offset + size` lies inside a file of `file_len`
/// bytes, without letting the sum wrap first.
fn check_range_fits(offset: u64, size: u64, file_len: u64) -> ZxResult {
    let end = offset.checked_add(size).ok_or(ZxError::INVALID_ARGS)?;
    if end > file_len {
        return Err(ZxError::INVALID_ARGS);
    }
    Ok(())
}

/// Extensional ELF loading methods for `VmAddressRegion`.
pub trait VmarExt {
    /// Create `VMObject` from all LOAD segments of `elf` and map them to this VMAR.
    /// Return the first `VMObject`.
    fn load_from_elf(&self, elf: &ElfFile) -> ZxResult<Arc<VmObject>>;
    /// Same as `load_from_elf`, but the `vmo` is an existing one instead of a lot of new ones.
    fn map_from_elf(&self, elf: &ElfFile, vmo: Arc<VmObject>) -> ZxResult;
}

impl VmarExt for VmAddressRegion {
    fn load_from_elf(&self, elf: &ElfFile) -> ZxResult<Arc<VmObject>> {
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            return self.load_from_elf_with_host_pages(elf);
        }

        #[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
        {
            let mut first_vmo = None;
            for ph in elf.program_iter() {
                // A `p_type` outside the handful of values `xmas_elf` knows
                // makes `get_type` an `Err`, and unwrapping it panicked the
                // kernel on a one-byte edit. Linux's own loader switches on
                // `p_type` and ignores what it does not recognise.
                if ph.get_type() != Ok(Type::Load) {
                    continue;
                }
                let vmo = make_vmo(elf, ph)?;
                let offset = ph.virtual_addr() as usize / PAGE_SIZE * PAGE_SIZE;
                let flags = ph.flags().to_mmu_flags();
                trace!("ph:{:#x?}, offset:{:#x?}, flags:{:#x?}", ph, offset, flags);
                //映射vmo物理内存块到 VMAR
                self.map_at(offset, vmo.clone(), 0, vmo.len(), flags)?;
                debug!("Map [{:x}, {:x})", offset, offset + vmo.len());
                first_vmo.get_or_insert(vmo);
            }
            // An ELF with no LOAD segment maps nothing, so there is no first
            // VMO to hand back; unwrapping the `None` panicked the kernel.
            first_vmo.ok_or(ZxError::INVALID_ARGS)
        }
    }
    fn map_from_elf(&self, elf: &ElfFile, vmo: Arc<VmObject>) -> ZxResult {
        for ph in elf.program_iter() {
            if ph.get_type() != Ok(Type::Load) {
                continue;
            }
            let offset = ph.virtual_addr() as usize;
            let flags = ph.flags().to_mmu_flags();
            let vmo_offset = pages(ph.physical_addr() as usize) * PAGE_SIZE;
            let len = pages(ph.mem_size() as usize) * PAGE_SIZE;
            self.map_at(offset, vmo.clone(), vmo_offset, len, flags)?;
        }
        Ok(())
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
trait HostPageElfExt {
    fn load_from_elf_with_host_pages(&self, elf: &ElfFile) -> ZxResult<Arc<VmObject>>;
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
impl HostPageElfExt for VmAddressRegion {
    fn load_from_elf_with_host_pages(&self, elf: &ElfFile) -> ZxResult<Arc<VmObject>> {
        // Fuchsia AArch64 ELF files use 4 KiB-aligned PT_LOAD boundaries, but
        // Apple Silicon only permits mmap at 16 KiB granularity.  Adjacent ELF
        // segments can consequently occupy the same host page.  Back the full
        // image with one VMO and map each run of host pages once, with the union
        // of all segment permissions that touch it.
        let size = elf.load_segment_size();
        let vmo = VmObject::new_paged(pages(size));
        let mut page_flags = vec![MMUFlags::empty(); pages(size)];

        for ph in elf.program_iter() {
            if ph.get_type() != Ok(Type::Load) {
                continue;
            }
            let data = match ph.get_data(elf).map_err(|_| ZxError::INVALID_ARGS)? {
                SegmentData::Undefined(data) => data,
                _ => return Err(ZxError::INVALID_ARGS),
            };
            let start = ph.virtual_addr() as usize;
            vmo.write(start, data)?;

            let first_page = start / PAGE_SIZE;
            let end_page = pages(start + ph.mem_size() as usize);
            let flags = ph.flags().to_mmu_flags();
            for page in &mut page_flags[first_page..end_page] {
                *page |= flags;
            }
        }

        let mut start_page = 0;
        while start_page < page_flags.len() {
            let flags = page_flags[start_page];
            let mut end_page = start_page + 1;
            while end_page < page_flags.len() && page_flags[end_page] == flags {
                end_page += 1;
            }
            if !flags.is_empty() {
                let offset = start_page * PAGE_SIZE;
                self.map_at(
                    offset,
                    vmo.clone(),
                    offset,
                    (end_page - start_page) * PAGE_SIZE,
                    flags,
                )?;
            }
            start_page = end_page;
        }
        Ok(vmo)
    }
}

trait FlagsExt {
    fn to_mmu_flags(&self) -> MMUFlags;
}

impl FlagsExt for Flags {
    fn to_mmu_flags(&self) -> MMUFlags {
        let mut flags = MMUFlags::USER;
        if self.is_read() {
            flags.insert(MMUFlags::READ);
        }
        if self.is_write() {
            flags.insert(MMUFlags::WRITE);
        }
        if self.is_execute() {
            flags.insert(MMUFlags::EXECUTE);
        }
        flags
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
/// Build the VMO backing one LOAD segment.
///
/// `load_from_elf` is the only caller and it has already filtered the segment
/// type, so there is no type check here: an `assert_eq!` on it used to stand
/// in this line, unreachable, next to the arithmetic that could actually be
/// reached with a hostile value.
fn make_vmo(elf: &ElfFile, ph: ProgramHeader) -> ZxResult<Arc<VmObject>> {
    let page_offset = ph.virtual_addr() as usize % PAGE_SIZE;
    // (VirtAddr余数 + MemSiz)的pages
    //
    // `mem_size` is a `u64` straight out of the file. `pages()` rounds up with
    // a `wrapping_add`, so a size near the top of the range used to come back
    // as a handful of pages -- a VMO far too small for the data about to be
    // written into it -- instead of failing.
    let pages = segment_pages(ph.mem_size(), page_offset)?;
    trace!(
        "VmObject new pages: {:#x}, virtual_addr: {:#x}",
        pages,
        page_offset
    );
    let vmo = VmObject::new_paged(pages);
    let data = match ph.get_data(elf).map_err(|_| ZxError::INVALID_ARGS)? {
        SegmentData::Undefined(data) => data,
        _ => return Err(ZxError::INVALID_ARGS),
    };
    //调用 VMObjectTrait.write, 分配物理内存，后写入程序数据
    vmo.write(page_offset, data)?;
    Ok(vmo)
}

/// Pages a LOAD segment needs, counting the part of its first page that sits
/// below `virtual_addr`.
///
/// `mem_size` is a `u64` the file chose, so it is bounded here rather than
/// handed to `pages()`, which rounds up with a `wrapping_add`. The bound is
/// the user address space itself, which is what Linux checks too
/// (`p_memsz > TASK_SIZE` is `EINVAL` in `load_elf_binary`): a segment that
/// could not be mapped whatever else is in the way is not worth allocating a
/// VMO for, and every size that passes rounds up without wrapping.
///
/// Public so the loader's tests can measure it directly: it is reached in
/// anger only through `load_from_elf`, and there every way of getting it
/// wrong comes back as the same `INVALID_ARGS` from a later step.
pub fn segment_pages(mem_size: u64, page_offset: usize) -> ZxResult<usize> {
    let bytes = mem_size
        .checked_add(page_offset as u64)
        .filter(|&bytes| bytes <= USER_ASPACE_SIZE)
        .ok_or(ZxError::INVALID_ARGS)?;
    Ok(pages(bytes as usize))
}

/// Pages an image must span for a LOAD segment that ends at
/// `virtual_addr + mem_size`, saturating rather than wrapping.
///
/// Both fields come from the file. A sum that does not fit is reported as the
/// largest page count there is, which the caller's allocation then refuses --
/// the old unchecked add wrapped to a small number and the allocation
/// succeeded, leaving the image mapped into a region far too short for it.
///
/// Public for the same reason as [`segment_pages`].
pub fn segment_end_pages(virtual_addr: u64, mem_size: u64) -> usize {
    let end = virtual_addr.saturating_add(mem_size);
    let end = usize::try_from(end).unwrap_or(usize::MAX);
    // `usize::MAX` rounds up to `usize::MAX / PAGE_SIZE + 1` pages, which is
    // exactly the count whose byte size overflows: saturate there instead.
    end.checked_add(PAGE_SIZE - 1)
        .map_or(usize::MAX / PAGE_SIZE, |rounded| rounded / PAGE_SIZE)
}

/// The bytes section `sh` stores in the file, or nothing when it stores none
/// that are there.
///
/// `SectionHeader::raw_data` unwraps `get_type()` -- an `Err` for any
/// `sh_type` the parser does not know -- asserts the section is not
/// `SHT_NULL`, and then slices the file with an unchecked add. This does none
/// of that.
fn section_bytes<'a>(elf: &ElfFile<'a>, sh: &SectionHeader) -> &'a [u8] {
    // A section that stores no bytes in the file has none to hand back,
    // whatever its `sh_offset` and `sh_size` say. Its size is also the one
    // `check_elf_bounds` cannot bound -- it describes memory, not the file --
    // so leaving this to the slice below would put the only guard on a number
    // nothing has checked.
    if sh.get_type() == Ok(ShType::NoBits) {
        return &[];
    }
    let start = sh.offset() as usize;
    let end = start.saturating_add(sh.size() as usize);
    // Every other section is bounded by `check_elf_bounds`, so this slice
    // exists; the fallback is there because nothing in the type system says so.
    elf.input.get(start..end).unwrap_or(&[])
}

/// The nul-terminated name at `offset` in a string table.
///
/// Public so the loader's tests can reach it: it is the one place in this file
/// whose whole job is to be the bounded version of something that panics, and
/// through its callers every failure looks like a section that was not found.
///
/// Every name lookup in `xmas_elf` goes through `zero::read_str`, which panics
/// twice over: `"No null byte in input"` when the table does not terminate,
/// and `"Non-utf8 string"` when the bytes are not UTF-8. Both are bytes the
/// file chose, so both were a kernel panic reachable from `execve`. Neither
/// can be checked from outside, because the check is what the panic replaces:
/// the scan has to be the bounded one.
pub fn str_at(table: &[u8], offset: u32) -> Option<&str> {
    let rest = table.get(offset as usize..)?;
    let len = rest.iter().position(|&b| b == 0)?;
    core::str::from_utf8(&rest[..len]).ok()
}

/// How many sections of `elf` can be read at all.
///
/// `e_shnum` is a `u16` the file chooses, but `SHN_LORESERVE` and everything
/// above it are escape values rather than section indices, so a table holds at
/// most that many however large the count says it is.
///
/// `ElfFile::section_header` knows neither bound. It `assert!`s on a reserved
/// index, and below one it slices `e_shoff + index * e_shentsize` out of the
/// file with no bounds check at all -- `e_shnum` included, so an index the
/// table does not contain is read exactly as if it did.
///
/// Neither is something [`check_elf_bounds`] can bound. It measures the table
/// by walking its entries, and the file that panicked is the one with no
/// entries to walk: with `e_shnum == 0` there is nothing to measure `e_shoff`
/// and `e_shentsize` against, and a 64-byte image whose `e_shentsize` is
/// `0xffff` was a kernel panic from `execve`. So the bound lives here, at the
/// read, where it holds for every file whether or not it was checked first.
fn section_count(elf: &ElfFile) -> u16 {
    elf.header.pt2.sh_count().min(SHN_LORESERVE)
}

/// The section at `index`, or nothing when the file has no such section.
fn section_at<'a>(elf: &ElfFile<'a>, index: u16) -> Option<SectionHeader<'a>> {
    if index >= section_count(elf) {
        return None;
    }
    elf.section_header(index).ok()
}

/// Every section of `elf`, stopping where the parser would `assert!` instead.
fn sections<'b, 'a>(elf: &'b ElfFile<'a>) -> impl Iterator<Item = SectionHeader<'a>> + 'b {
    (0..section_count(elf)).filter_map(move |index| elf.section_header(index).ok())
}

/// The section header table's own string table, which holds section names.
fn section_names<'a>(elf: &ElfFile<'a>) -> &'a [u8] {
    match section_at(elf, elf.header.pt2.sh_str_index()) {
        Some(names) => section_bytes(elf, &names),
        None => &[],
    }
}

/// Find a section by name, without going through [`str_at`]'s panicking
/// counterpart in the parser.
fn find_section<'a>(elf: &ElfFile<'a>, name: &str) -> Option<SectionHeader<'a>> {
    let names = section_names(elf);
    sections(elf).find(|sh| str_at(names, sh.name()) == Some(name))
}

/// Size and file alignment of one entry of a section `xmas_elf` hands back as
/// a slice, for the types this loader asks for.
///
/// The alignment is a property of where the section sits in the file, not just
/// of the struct: `zero::read_array` builds the slice with
/// `slice::from_raw_parts` straight off the section's bytes, and every entry
/// type here carries fields as wide as the class. A real linker gives these
/// sections an `sh_addralign` to match, but `sh_offset` is a number the file
/// chooses and nothing makes it honour it.
fn section_entry_layout(elf: &ElfFile, ty: ShType) -> Option<(usize, usize)> {
    let is_64 = matches!(elf.header.pt1.class(), Class::SixtyFour);
    let align = if is_64 { 8 } else { 4 };
    let size = match ty {
        ShType::SymTab | ShType::DynSym => {
            if is_64 {
                24
            } else {
                16
            }
        }
        ShType::Rela => {
            if is_64 {
                24
            } else {
                12
            }
        }
        ShType::Rel => {
            if is_64 {
                16
            } else {
                8
            }
        }
        _ => return None,
    };
    Some((size, align))
}

/// A section's contents, as the type the caller asked for.
///
/// Three checks the parser leaves out. It dispatches on `sh_type` alone, so
/// asking it for `.dynsym` and being handed a note section, or a group, or a
/// 32-bit note it answers with `unimplemented!()`, is a matter of what the
/// file says; naming the expected type here means only the types this loader
/// understands are ever parsed.
///
/// The other two are what `zero::read_array` needs to build the slice and does
/// not check. It ASSERTS that the section divides exactly into entries -- a
/// `.dynsym` one byte short of a whole symbol panicked the kernel -- and it
/// then calls `slice::from_raw_parts` on the section's first byte, where a
/// misaligned address is undefined behaviour however whole the entries are.
/// That one is worse than the panic beside it: the debug build's precondition
/// check for it does not unwind, so the machine aborts where a kernel cannot
/// even report what happened. The address is the image's own plus the offset
/// the file chose, so both go into the check.
fn section_data<'a>(
    elf: &ElfFile<'a>,
    sh: &SectionHeader<'a>,
    expected: ShType,
) -> Option<SectionData<'a>> {
    if sh.get_type() != Ok(expected) {
        return None;
    }
    if let Some((entry_size, align)) = section_entry_layout(elf, expected) {
        if !(sh.size() as usize).is_multiple_of(entry_size) {
            warn!(
                "elf: section of {} bytes does not divide into {}-byte entries",
                sh.size(),
                entry_size
            );
            return None;
        }
        let at = (elf.input.as_ptr() as usize).wrapping_add(sh.offset() as usize);
        if !at.is_multiple_of(align) {
            warn!(
                "elf: section at offset {:#x} is not aligned to {} bytes",
                sh.offset(),
                align
            );
            return None;
        }
    }
    sh.get_data(elf).ok()
}

/// Extensional ELF loading methods for `ElfFile`.
pub trait ElfExt {
    /// Get total size of all LOAD segments.
    fn load_segment_size(&self) -> usize;
    /// Get address of the given `symbol`.
    fn get_symbol_address(&self, symbol: &str) -> Option<u64>;
    /// Get the program interpreter path name.
    fn get_interpreter(&self) -> Result<&str, &str>;
    /// Get address of elf phdr
    fn get_phdr_vaddr(&self) -> Option<u64>;
    /// Get the symbol table for dynamic linking (.dynsym section).
    fn dynsym(&self) -> Result<&[DynEntry64], &'static str>;
    /// Relocate according to the dynamic relocation section (.rel.dyn section).
    ///
    /// `scratch_vmar` is where an `R_X86_64_IRELATIVE` entry (if any) borrows a
    /// throwaway page for its resolver's stack. It must NOT be `vmar` itself
    /// when `vmar` is a tightly-packed sub-region (as both call sites' interp/
    /// image VMARs are, sized to their LOAD segments with zero spare room) —
    /// pass the PARENT address space, which still has free space to carve a
    /// page from. Unused (and fine to pass the same VMAR) on any binary with no
    /// IRELATIVE relocations, i.e. every non-glibc / non-dynamically-linked one.
    fn relocate(
        &self,
        vmar: Arc<VmAddressRegion>,
        scratch_vmar: &Arc<VmAddressRegion>,
    ) -> Result<(), &'static str>;
}

/// Where a relocation writes: `base + r_offset`, or nothing when that address
/// does not exist.
///
/// `r_offset` is a `u64` straight out of the file and `base` is wherever the
/// image was mapped, so their sum was an unchecked add reached once per
/// relocation entry. Every dynamically linked program takes this path, because
/// the loader relocates the ELF interpreter itself, and that one is mapped
/// after the main image -- at a base that is not zero, which is what it takes
/// for the add to overflow. In debug that panicked the kernel from `execve`;
/// in release it wrapped, and the wrapped address could still land inside a
/// real mapping of the same process, so the relocation quietly wrote a word
/// somewhere it was never meant to go. An address outside the address space is
/// something this loop already has an answer for.
fn reloc_addr(base: usize, offset: u64) -> Result<usize, &'static str> {
    usize::try_from(offset)
        .ok()
        .and_then(|offset| base.checked_add(offset))
        .ok_or("relocation target outside the address space")
}

impl ElfExt for ElfFile<'_> {
    fn load_segment_size(&self) -> usize {
        self.program_iter()
            .filter(|ph| ph.get_type() == Ok(Type::Load))
            // Both fields come from the file, so their sum is the loader's to
            // bound: it used to be an unchecked `u64` add, and `pages()` then
            // wrapped whatever came out of it.
            .map(|ph| segment_end_pages(ph.virtual_addr(), ph.mem_size()))
            .max()
            .unwrap_or(0)
            * PAGE_SIZE
    }

    fn get_symbol_address(&self, symbol: &str) -> Option<u64> {
        // Every section, asked for its contents whatever its type, used to run
        // through the parser's whole dispatch: a group section indexed
        // `data[0]` on an empty one, a 32-bit note reached an
        // `unimplemented!()`, and anything it hands back as a slice asserted
        // on the section's size. Only symbol tables are of any interest here.
        let names = find_section(self, ".strtab")
            .map(|strtab| section_bytes(self, &strtab))
            .unwrap_or(&[]);
        for section in sections(self) {
            if let Some(SectionData::SymbolTable64(entries)) =
                section_data(self, &section, ShType::SymTab)
            {
                for e in entries {
                    if str_at(names, e.name()) == Some(symbol) {
                        return Some(e.value());
                    }
                }
            }
        }
        None
    }

    fn get_interpreter(&self) -> Result<&str, &str> {
        let header = self
            .program_iter()
            .find(|ph| ph.get_type() == Ok(Type::Interp))
            .ok_or("no interp header")?;
        let data = match header.get_data(self)? {
            SegmentData::Undefined(data) => data,
            _ => return Err("bad interp"),
        };
        // An interpreter path with no terminator ran this scan off the end of
        // the segment and panicked the kernel.
        let len = data
            .iter()
            .position(|&b| b == 0)
            .ok_or("interp path is not nul-terminated")?;
        let path = core::str::from_utf8(&data[..len]).map_err(|_| "failed to convert to utf8")?;
        Ok(path)
    }

    /*
     * [ ERROR ] page fualt from user mode 0x40 READ
     */

    fn get_phdr_vaddr(&self) -> Option<u64> {
        if let Some(phdr) = self
            .program_iter()
            .find(|ph| ph.get_type() == Ok(Type::Phdr))
        {
            // if phdr exists in program header, use it
            Some(phdr.virtual_addr())
        } else if let Some(elf_addr) = self
            .program_iter()
            .find(|ph| ph.get_type() == Ok(Type::Load) && ph.offset() == 0)
        {
            // otherwise, check if elf is loaded from the beginning, then phdr can be inferred.
            //
            // Both halves come from the file, and this used to be an unchecked
            // `u64` add: a `PT_LOAD` at file offset 0 with a `p_vaddr` near the
            // top of the range panicked the kernel here, from `execve` on. An
            // address that does not exist is no phdr, which is a case this
            // already has: the loader leaves `AT_PHDR` out.
            let inferred = elf_addr
                .virtual_addr()
                .checked_add(self.header.pt2.ph_offset());
            if inferred.is_none() {
                warn!("elf: inferred phdr address does not fit, tls might not work");
            }
            inferred
        } else {
            warn!("elf: no phdr found, tls might not work");
            None
        }
    }

    fn dynsym(&self) -> Result<&[DynEntry64], &'static str> {
        let section = find_section(self, ".dynsym").ok_or(".dynsym not found")?;
        match section_data(self, &section, ShType::DynSym).ok_or("corrupted .dynsym")? {
            SectionData::DynSymbolTable64(dsym) => Ok(dsym),
            _ => Err("bad .dynsym"),
        }
    }

    #[allow(unsafe_code)]
    // `scratch_vmar` only feeds `resolve_irelative_x86_64`, which is gated to
    // x86_64 (IFUNC resolution means *executing* the resolver, and only the
    // x86_64 trap plumbing to do that exists). On every other arch the
    // parameter is genuinely unused, and the crate's `#[deny(warnings)]` turns
    // that into a build failure — aarch64 and riscv64 did not compile at all.
    // Scoped to those arches so the lint keeps its teeth on x86_64.
    #[cfg_attr(not(target_arch = "x86_64"), allow(unused_variables))]
    fn relocate(
        &self,
        vmar: Arc<VmAddressRegion>,
        scratch_vmar: &Arc<VmAddressRegion>,
    ) -> Result<(), &'static str> {
        // Symbol-resolving relocations (write `S + A`).
        // x86_64
        const REL_GOT: u32 = 6; // R_X86_64_GLOB_DAT
        const REL_PLT: u32 = 7; // R_X86_64_JUMP_SLOT
        const R_X86_64_64: u32 = 1;
        // riscv64
        const R_RISCV_64: u32 = 2;
        // aarch64
        const R_AARCH64_GLOBAL_DATA: u32 = 0x401;
        const R_AARCH64_JUMP_SLOT: u32 = 0x402;

        // Base-relative relocations (write `B + A`).
        // x86_64
        const REL_RELATIVE: u32 = 8; // R_X86_64_RELATIVE
                                     // riscv64
        const R_RISCV_RELATIVE: u32 = 3;
        // aarch64
        const R_AARCH64_RELATIVE: u32 = 0x403;

        // R_X86_64_IRELATIVE: the value to write is not a fixed address but the
        // RETURN VALUE of calling a resolver function at `base + addend` (an
        // IFUNC). glibc's own dynamic linker (`ld-linux-x86-64.so.2`) uses
        // CPU-feature-dispatched routines (optimized memcpy/memset/strlen etc.)
        // even internally, so a real glibc interpreter's `.rela.plt` legitimately
        // carries these. Collected here and resolved in a batch after the main
        // relocation loop (see the call to `resolve_irelative_x86_64` below) —
        // resolving one requires actually EXECUTING the resolver, which this
        // per-entry loop has no business doing mid-scan.
        #[cfg(target_arch = "x86_64")]
        const R_X86_64_IRELATIVE: u32 = 37;
        #[cfg(target_arch = "x86_64")]
        let mut irelative: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();

        let base = vmar.addr();
        // `.dynsym` may be absent for binaries that only carry RELATIVE
        // relocations; resolve it lazily so those still get applied.
        let dynsym = self.dynsym().ok();
        let mut found_any = false;

        // One-entry mapping cache. Relocation targets are heavily clustered
        // (GOT / PLT / .data.rel.ro live in one or two mappings), while
        // `Vmar::write_memory` re-resolves the mapping with a linear VMAR
        // scan on EVERY call — O(entries × mappings) per exec for a PIE
        // binary with thousands of R_*_RELATIVE entries.
        let mut cached: Option<Arc<VmMapping>> = None;
        let write_word = |cached: &mut Option<Arc<VmMapping>>,
                          addr: usize,
                          value: usize|
         -> Result<(), &'static str> {
            let bytes = value.to_ne_bytes();
            if let Some(map) = cached.as_ref() {
                if map
                    .write_memory_if_contains(addr, &bytes)
                    .map_err(|_| "Invalid Vmar")?
                {
                    return Ok(());
                }
            }
            let map = vmar.find_mapping(addr).ok_or("Invalid Vmar")?;
            if !map
                .write_memory_if_contains(addr, &bytes)
                .map_err(|_| "Invalid Vmar")?
            {
                // Boundary case (write straddles the mapping end): preserve the
                // old clamped-partial-write behaviour.
                vmar.write_memory(addr, &bytes)
                    .map_err(|_| "Invalid Vmar")?;
            }
            *cached = Some(map);
            Ok(())
        };

        // Apply both the general dynamic relocations (`.rela.dyn`) and the PLT
        // relocations (`.rela.plt`). The latter holds the JUMP_SLOT entries that
        // back the procedure linkage table; skipping it leaves call targets
        // pointing at unrelocated stubs (observed as a jump to a low address and
        // an Invalid Opcode #UD fault).
        // Names for the log line below, read the bounded way.
        let dynstr = find_section(self, ".dynstr")
            .map(|dynstr| section_bytes(self, &dynstr))
            .unwrap_or(&[]);
        for &sec_name in [".rela.dyn", ".rela.plt"].iter() {
            let section = match find_section(self, sec_name) {
                Some(section) => section,
                None => continue,
            };
            let entries = match section_data(self, &section, ShType::Rela) {
                Some(SectionData::Rela64(entries)) => entries,
                Some(_) => continue,
                None => return Err("corrupted relocation section"),
            };
            found_any = true;
            for entry in entries.iter() {
                match entry.get_type() {
                    REL_GOT
                    | REL_PLT
                    | R_X86_64_64
                    | R_RISCV_64
                    | R_AARCH64_GLOBAL_DATA
                    | R_AARCH64_JUMP_SLOT => {
                        let dynsym = match dynsym {
                            Some(dynsym) => dynsym,
                            None => {
                                warn!("relocate: symbol relocation but no .dynsym; skipping");
                                continue;
                            }
                        };
                        // The index is a `u32` the file chose, and indexing
                        // the slice with it panicked the kernel on any entry
                        // naming a symbol past the end of `.dynsym`.
                        let sym = match dynsym.get(entry.get_symbol_table_index() as usize) {
                            Some(sym) => sym,
                            None => {
                                warn!(
                                    "relocate: symbol index {} is outside .dynsym ({} entries)",
                                    entry.get_symbol_table_index(),
                                    dynsym.len()
                                );
                                continue;
                            }
                        };
                        // An undefined symbol (shndx == 0) is resolved later by the
                        // dynamic linker in user space (or is simply unavailable to
                        // the in-kernel loader). Skip it instead of panicking — a
                        // user binary must never be able to crash the kernel.
                        if sym.shndx() == 0 {
                            let name = str_at(dynstr, sym.name()).unwrap_or("<unknown>");
                            warn!("relocate: undefined symbol {:?}, skipping", name);
                            continue;
                        }
                        // `S + A`. The VALUE is ABI arithmetic on numbers
                        // the file chose and is allowed to wrap: a negative
                        // `r_addend` is ordinary in a real object, and the
                        // dynamic linkers this loader stands in for compute it
                        // in plain C. Only the ADDRESS has to exist.
                        let symval = base.wrapping_add(sym.value() as usize);
                        let value = symval.wrapping_add(entry.get_addend() as usize);
                        let addr = reloc_addr(base, entry.get_offset())?;
                        trace!("GOT write: {:#x} @ {:#x}", value, addr);
                        write_word(&mut cached, addr, value)?;
                    }
                    REL_RELATIVE | R_RISCV_RELATIVE | R_AARCH64_RELATIVE => {
                        // `B + A`, same split as above: the value may wrap,
                        // the address may not.
                        let value = base.wrapping_add(entry.get_addend() as usize);
                        let addr = reloc_addr(base, entry.get_offset())?;
                        trace!("RELATIVE write: {:#x} @ {:#x}", value, addr);
                        write_word(&mut cached, addr, value)?;
                    }
                    // Unsupported relocation type (e.g. TLS relocations). Log and
                    // skip rather than `unimplemented!()`, which would panic the
                    // whole kernel because of one user program.
                    other => {
                        #[cfg(target_arch = "x86_64")]
                        if other == R_X86_64_IRELATIVE {
                            let got_addr = reloc_addr(base, entry.get_offset())?;
                            // The resolver is CALLED, so unlike a plain value
                            // it has to be an address that exists; the scratch
                            // mapping rejects it otherwise.
                            let resolver_addr = base.wrapping_add(entry.get_addend() as usize);
                            irelative.push((got_addr, resolver_addr));
                            continue;
                        }
                        warn!(
                            "relocate: skipping unsupported relocation type {} in {}",
                            other, sec_name
                        );
                    }
                }
            }
        }

        // Resolve every collected IRELATIVE entry by actually running its
        // resolver, then write the results back through the same `write_word`
        // path as every other relocation type.
        #[cfg(target_arch = "x86_64")]
        if !irelative.is_empty() {
            let n = irelative.len();
            match resolve_irelative_x86_64(scratch_vmar, &irelative) {
                Ok(resolved) => {
                    let got = resolved.len();
                    for (addr, value) in resolved {
                        write_word(&mut cached, addr, value)?;
                    }
                    debug!("relocate: resolved {}/{} IRELATIVE entries", got, n);
                }
                Err(e) => {
                    warn!(
                        "relocate: IRELATIVE batch resolution failed ({}); {} entries left unresolved",
                        e, n
                    );
                }
            }
        }

        if found_any {
            Ok(())
        } else {
            Err(".rela.dyn not found")
        }
    }
}

/// Resolve a batch of `R_X86_64_IRELATIVE` relocations by actually EXECUTING
/// each resolver function once, synchronously, in a brief ring-3 excursion into
/// the target address space. Unlike every other relocation type, IRELATIVE's
/// value is not a fixed address — it is the RETURN VALUE of *calling* the
/// resolver at `base + addend`, exactly what a real ld.so does for its own
/// IFUNCs (glibc's `elf_ifunc_invoke`). The kernel does it here because this
/// loader eagerly relocates the ELF interpreter itself in kernel space, before
/// it ever runs its own bootstrap — real Linux never needs this, since ld.so
/// self-relocates in userspace and resolves its own IFUNCs as ordinary running
/// C code.
///
/// Leaving these unresolved (the previous behaviour: skip + warn) left the GOT
/// slot at zero, so any call through it jumped to address 0. That is silent
/// death for a glibc-linked desktop stack specifically: the interpreter
/// bootstraps far enough to load and run the main program (labwc logs
/// correctly through its whole configuration/output-setup phase — none of
/// which happens to call a broken slot), but the first render pass — heavy on
/// memcpy/memset for the software (pixman) blit path, both IFUNC-resolved in
/// glibc — hits one and the process goes silently dark: modeset succeeds, the
/// log stops dead, nothing is ever drawn. Confirmed by reproducing the exact
/// symptom in QEMU with a glibc labwc/wlroots stack and tracing the fault to
/// `relocate: skipping unsupported relocation type 37 in .rela.plt`.
///
/// A resolver is a small ABI-guaranteed leaf function (checks CPUID/HWCAP,
/// returns a function pointer; no syscalls, no callbacks into the loader, no
/// blocking), so running it via the SAME synchronous user-mode entry primitive
/// every thread already uses (`UserContext::enter_uspace`) is safe: any trap
/// other than the deliberately-unmapped sentinel return address we set up
/// (a clean instruction-fetch page fault, chosen so `ret` cannot be confused
/// with a legitimate jump) aborts the WHOLE batch rather than writing a
/// possibly-bogus value into a live GOT slot — the same fail-closed contract
/// `relocate()` already has for every other error path. A real hardware
/// interrupt (the 250 Hz timer) is serviced and the SAME resolver call is
/// simply resumed, exactly as the normal thread-execution loop
/// (`loader/src/linux.rs::run_user`) already does for ordinary user code.
#[cfg(target_arch = "x86_64")]
fn resolve_irelative_x86_64(
    vmar: &Arc<VmAddressRegion>,
    entries: &[(usize, usize)],
) -> Result<alloc::vec::Vec<(usize, usize)>, &'static str> {
    use kernel_hal::context::{TrapReason, UserContext, UserContextField};

    // Deliberately unmapped, canonical, and cheap to recognise: `ret` popping
    // this into RIP takes an immediate instruction-fetch #PF at EXACTLY this
    // address, which is how a resolver's return is detected. Address 0 is
    // avoided (a NULL-derived bug elsewhere could coincidentally target it).
    const SENTINEL_RETURN: usize = 0x8;

    // A scratch stack: this runs before the process has a real stack (that is
    // allocated by the caller AFTER this relocation pass), so borrow one page
    // from the interpreter's own VMAR for the duration of this call, then give
    // it back. One word (the sentinel return address) is all any of these
    // leaf resolvers need.
    let scratch = vmar
        .map(
            None,
            VmObject::new_paged(1),
            0,
            PAGE_SIZE,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
        )
        .map_err(|_| "IRELATIVE: failed to map scratch stack")?;
    let scratch_top = scratch + PAGE_SIZE;
    let sp = scratch_top - core::mem::size_of::<usize>();
    let unmap_scratch = || {
        let _ = vmar.unmap(scratch, PAGE_SIZE);
    };
    if vmar
        .write_memory(sp, &SENTINEL_RETURN.to_ne_bytes())
        .is_err()
    {
        unmap_scratch();
        return Err("IRELATIVE: failed to seed scratch stack");
    }

    let mut resolved = alloc::vec::Vec::with_capacity(entries.len());
    // One CR3 write for the whole batch rather than one per entry; nothing
    // between entries needs the kernel's own address space.
    kernel_hal::vm::activate_paging(vmar.table_phys());

    let mut aborted = false;
    for &(got_addr, resolver_addr) in entries {
        let mut ctx = UserContext::new();
        ctx.setup_uspace(resolver_addr, sp, &[0, 0, 0]);
        // Loop instead of a single `enter_uspace`: a real hardware interrupt
        // (the periodic timer) legitimately traps here too. Service it and
        // resume the SAME context exactly where it left off — x86 interrupts
        // are precise, so this is indistinguishable from the interrupt never
        // having happened, from the resolver's point of view.
        loop {
            ctx.enter_uspace();
            match ctx.trap_reason() {
                TrapReason::Interrupt(vector) => {
                    kernel_hal::interrupt::handle_irq(vector);
                    continue;
                }
                TrapReason::PageFault(vaddr, flags)
                    if vaddr == SENTINEL_RETURN && flags.contains(MMUFlags::EXECUTE) =>
                {
                    resolved.push((got_addr, ctx.get_field(UserContextField::ReturnValue)));
                    break;
                }
                other => {
                    warn!(
                        "relocate: IRELATIVE resolver at {:#x} did not return cleanly ({:?}); \
                         aborting the remaining {} entr{} in this batch",
                        resolver_addr,
                        other,
                        entries.len() - resolved.len(),
                        if entries.len() - resolved.len() == 1 {
                            "y"
                        } else {
                            "ies"
                        }
                    );
                    aborted = true;
                    break;
                }
            }
        }
        if aborted {
            break;
        }
    }

    kernel_hal::vm::activate_kernel_paging();
    unmap_scratch();
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;

    /// `PT_LOAD`, `PT_INTERP`, `PT_PHDR`, and a type no ELF standard names.
    const PT_LOAD: u32 = 1;
    const PT_INTERP: u32 = 3;
    const PT_PHDR: u32 = 6;
    /// Between `PT_TLS` and `PT_LOOS`, so `get_type` answers `Err` for it --
    /// which is the value this loader used to `unwrap`.
    const PT_UNKNOWN: u32 = 0x42;
    /// `PF_R | PF_X`.
    const PF_RX: u32 = 4 | 1;

    /// `SHT_PROGBITS`, `SHT_STRTAB` and `SHT_NOBITS`.
    const SHT_PROGBITS: u32 = 1;
    const SHT_STRTAB: u32 = 3;
    const SHT_NOBITS_U32: u32 = 8;

    /// Room for every image built here; the largest is a header, two program
    /// headers, three sections and a short interpreter path.
    const IMAGE_CAP: usize = 512;

    /// The backing store of a synthetic image.
    ///
    /// A `Vec<u8>` asks its allocator for an alignment of one, so an image
    /// built in one would be aligned, or not, by luck -- and part of what is
    /// under test here is an alignment rule. Aligned to eight, a test sees
    /// exactly the skew it asked for and no other.
    #[repr(align(8))]
    struct Buffer([u8; IMAGE_CAP]);

    /// A little-endian ELF image, assembled field by field.
    struct Image {
        buf: Box<Buffer>,
        /// Where in [`Buffer`] the image starts. Zero everywhere but in the
        /// tests about the address the image itself sits at.
        skew: usize,
        /// How many of its bytes the loader is shown.
        len: usize,
        /// Offset of `e_phoff`; the class decides it and everything after it.
        ph_offset_at: usize,
        /// Offset of `e_phentsize`.
        ph_entry_size_at: usize,
        /// Width of one offset field, so one setter serves both classes.
        offset_width: usize,
    }

    impl Image {
        /// A 64-bit header with no tables: the fields `ElfFile::new` reads,
        /// and nothing else.
        fn elf64() -> Self {
            Self::elf64_at(0)
        }

        /// The same image, starting `skew` bytes into an aligned buffer.
        fn elf64_at(skew: usize) -> Self {
            let mut img = Image {
                buf: Box::new(Buffer([0; IMAGE_CAP])),
                skew,
                len: 64,
                ph_offset_at: 32,
                ph_entry_size_at: 54,
                offset_width: 8,
            };
            img.put(0, &ELF_MAGIC);
            img.put8(ELF_CLASS_AT, ELF_CLASS64);
            img.put8(ELF_DATA_AT, ELF_DATA_LSB);
            img.put8(6, 1); // EI_VERSION
            img.put16(16, 2); // e_type = ET_EXEC
            img.put16(18, 0x3e); // e_machine = EM_X86_64
            img.put16(52, 64); // e_ehsize
            img.put16(54, 56); // e_phentsize
            img.put16(58, 64); // e_shentsize
            img
        }

        /// A 32-bit header, whose fields sit somewhere else entirely.
        fn elf32_at(skew: usize) -> Self {
            let mut img = Image {
                buf: Box::new(Buffer([0; IMAGE_CAP])),
                skew,
                len: 52,
                ph_offset_at: 28,
                ph_entry_size_at: 42,
                offset_width: 4,
            };
            img.put(0, &ELF_MAGIC);
            img.put8(ELF_CLASS_AT, ELF_CLASS32);
            img.put8(ELF_DATA_AT, ELF_DATA_LSB);
            img.put8(6, 1);
            img.put16(16, 2);
            img.put16(18, 3); // EM_386
            img.put16(40, 52); // e_ehsize
            img.put16(42, 32); // e_phentsize
            img.put16(46, 40); // e_shentsize
            img
        }

        fn put(&mut self, at: usize, bytes: &[u8]) {
            let at = self.skew + at;
            self.buf.0[at..at + bytes.len()].copy_from_slice(bytes);
        }
        fn put8(&mut self, at: usize, v: u8) {
            self.put(at, &[v]);
        }
        fn put16(&mut self, at: usize, v: u16) {
            self.put(at, &v.to_le_bytes());
        }
        fn put32(&mut self, at: usize, v: u32) {
            self.put(at, &v.to_le_bytes());
        }
        fn put64(&mut self, at: usize, v: u64) {
            self.put(at, &v.to_le_bytes());
        }
        /// An offset field, as wide as this image's class.
        fn put_off(&mut self, at: usize, v: u64) {
            match self.offset_width {
                4 => self.put32(at, v as u32),
                _ => self.put64(at, v),
            }
        }

        fn phoff(&mut self, v: u64) {
            self.put_off(self.ph_offset_at, v);
        }
        fn shoff(&mut self, v: u64) {
            self.put_off(self.ph_offset_at + self.offset_width, v);
        }
        fn phentsize(&mut self, v: u16) {
            self.put16(self.ph_entry_size_at, v);
        }
        fn phnum(&mut self, v: u16) {
            self.put16(self.ph_entry_size_at + 2, v);
        }
        fn shentsize(&mut self, v: u16) {
            self.put16(self.ph_entry_size_at + 4, v);
        }
        fn shnum(&mut self, v: u16) {
            self.put16(self.ph_entry_size_at + 6, v);
        }
        fn shstrndx(&mut self, v: u16) {
            self.put16(self.ph_entry_size_at + 8, v);
        }

        /// One 64-bit program header at file offset `at`.
        fn program_header(
            &mut self,
            at: usize,
            ty: u32,
            offset: u64,
            vaddr: u64,
            sizes: (u64, u64),
        ) {
            let (filesz, memsz) = sizes;
            self.put32(at, ty);
            self.put32(at + 4, PF_RX);
            self.put64(at + 8, offset);
            self.put64(at + 16, vaddr);
            self.put64(at + 24, vaddr); // p_paddr
            self.put64(at + 32, filesz);
            self.put64(at + 40, memsz);
            self.put64(at + 48, 0x1000); // p_align
        }

        /// One 64-bit section header at file offset `at`.
        fn section_header(&mut self, at: usize, name: u32, ty: u32, offset: u64, size: u64) {
            self.put32(at, name);
            self.put32(at + 4, ty);
            self.put64(at + 24, offset);
            self.put64(at + 32, size);
        }

        /// How many bytes of the image the loader is shown.
        fn shown(&mut self, len: usize) {
            self.len = len;
        }

        fn bytes(&self) -> &[u8] {
            &self.buf.0[self.skew..self.skew + self.len]
        }
    }

    /// Shorthand for the one error every rejection here answers with.
    fn refused() -> ZxResult {
        Err(ZxError::INVALID_ARGS)
    }

    /// A 64-bit image with one `PT_LOAD` segment, shown whole.
    ///
    /// The segment's bytes are the 16 at file offset 0x80, so the image is
    /// header + one program header + a page's worth of room for data.
    fn one_load_segment(vaddr: u64) -> Image {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_LOAD, 0x80, vaddr, (16, 16));
        img.put(0x80, b"0123456789abcdef");
        img.shown(0x90);
        img
    }

    // ---- the header itself -------------------------------------------------

    #[test]
    fn a_well_formed_image_passes() {
        let img = one_load_segment(0x20_0000);
        assert_eq!(check_elf_bounds(img.bytes()), Ok(()));
        assert!(parse_checked_elf(img.bytes()).is_ok());
    }

    #[test]
    fn a_file_that_is_not_an_elf_is_refused() {
        assert_eq!(check_elf_bounds(b"#!/bin/sh\necho hi\n"), refused());
        // A shell script fails several ways at once. This one is a perfectly
        // good image with one byte of its magic changed, so the magic is the
        // only thing that can refuse it.
        let mut img = Image::elf64();
        img.put8(2, b'l');
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_file_shorter_than_the_magic_is_refused() {
        assert_eq!(check_elf_bounds(&ELF_MAGIC[..3]), refused());
    }

    #[test]
    fn a_file_of_nothing_but_the_magic_is_refused() {
        // Five bytes: magic and a class, with no `EI_DATA` behind it. The
        // class byte is read before the length is checked against the class's
        // header size, so the length has to be checked first.
        assert_eq!(
            check_elf_bounds(&[0x7f, b'E', b'L', b'F', ELF_CLASS64]),
            refused()
        );
    }

    #[test]
    fn a_big_endian_image_is_refused() {
        let mut img = Image::elf64();
        img.put8(ELF_DATA_AT, 2); // ELFDATA2MSB
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_class_the_parser_does_not_know_is_refused() {
        for class in [0u8, 3, 0xff] {
            let mut img = Image::elf64();
            img.put8(ELF_CLASS_AT, class);
            assert_eq!(check_elf_bounds(img.bytes()), refused(), "class {}", class);
        }
    }

    #[test]
    fn a_header_one_byte_short_of_its_own_class_is_refused() {
        // The 20-byte file that used to panic the parser, and its 32-bit twin.
        let mut img = Image::elf64();
        img.shown(63);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
        let mut img = Image::elf32_at(0);
        img.shown(51);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_header_of_exactly_its_own_size_passes() {
        assert_eq!(check_elf_bounds(Image::elf64().bytes()), Ok(()));
        assert_eq!(check_elf_bounds(Image::elf32_at(0).bytes()), Ok(()));
    }

    // ---- where each class keeps the fields this check reads ----------------

    #[test]
    fn each_class_reads_its_table_fields_where_that_class_puts_them() {
        for (mut img, class) in [
            (Image::elf64(), ELF_CLASS64),
            (Image::elf32_at(0), ELF_CLASS32),
        ] {
            // Six distinct values, so a field read from the wrong offset
            // cannot come back looking right.
            img.phoff(0x11);
            img.shoff(0x22);
            img.phentsize(0x33);
            img.phnum(0x44);
            img.shentsize(0x55);
            img.shnum(0x66);
            let layout = HeaderLayout::of_class(class).unwrap();
            let ph = TableExtent::read(img.bytes(), &layout, 0).unwrap();
            let sh = TableExtent::read(img.bytes(), &layout, 1).unwrap();
            assert_eq!((ph.offset, ph.entry_size, ph.count), (0x11, 0x33, 0x44));
            assert_eq!((sh.offset, sh.entry_size, sh.count), (0x22, 0x55, 0x66));
        }
    }

    // ---- the program header table -------------------------------------------

    #[test]
    fn a_program_header_table_past_the_end_of_the_file_is_refused() {
        let mut img = Image::elf64();
        img.phoff(0x1000);
        img.phnum(1);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_program_header_table_one_byte_too_long_is_refused() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.shown(119);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_program_header_table_that_ends_at_the_last_byte_passes() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_LOAD, 0, 0, (0, 0));
        img.shown(120);
        assert_eq!(check_elf_bounds(img.bytes()), Ok(()));
    }

    #[test]
    fn a_program_header_entry_smaller_than_the_struct_the_parser_reads_is_refused() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.phentsize(55);
        img.shown(256);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn no_program_headers_means_the_table_is_never_measured() {
        // With no entries to walk there is nothing to measure `e_phoff` and
        // `e_phentsize` against, and nothing reads them either.
        let mut img = Image::elf64();
        img.phoff(u64::MAX);
        img.phentsize(0xffff);
        img.phnum(0);
        assert_eq!(check_elf_bounds(img.bytes()), Ok(()));
    }

    #[test]
    fn a_program_header_table_whose_extent_wraps_is_refused() {
        let mut img = Image::elf64();
        img.phoff(u64::MAX - 63);
        img.phnum(1);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    // ---- what each segment says about the file -----------------------------

    #[test]
    fn a_segment_whose_bytes_run_past_the_end_is_refused() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_LOAD, 0, 0, (0x10000, 0x10000));
        img.shown(120);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_segment_whose_offset_and_size_wrap_is_refused() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_LOAD, u64::MAX, 0, (1, 1));
        img.shown(120);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_segment_that_stores_no_bytes_may_sit_at_the_end_of_the_file() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_LOAD, 120, 0, (0, 0x1000));
        img.shown(120);
        assert_eq!(check_elf_bounds(img.bytes()), Ok(()));
    }

    // ---- the section header table ------------------------------------------

    #[test]
    fn a_section_header_table_past_the_end_of_the_file_is_refused() {
        let mut img = Image::elf64();
        img.shoff(0x1000);
        img.shnum(1);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_section_header_entry_smaller_than_the_struct_is_refused() {
        let mut img = Image::elf64();
        img.shoff(64);
        img.shnum(1);
        img.shentsize(63);
        img.shown(256);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn no_sections_means_a_nonsense_entry_size_is_never_read() {
        // A 64-byte image whose `e_shentsize` is `0xffff` and whose `e_shnum`
        // is zero: there is no table, so there is nothing to bound.
        let mut img = Image::elf64();
        img.shoff(0);
        img.shentsize(0xffff);
        img.shnum(0);
        assert_eq!(check_elf_bounds(img.bytes()), Ok(()));
    }

    #[test]
    fn a_section_whose_bytes_run_past_the_end_is_refused() {
        let mut img = Image::elf64();
        img.shoff(64);
        img.shnum(1);
        img.section_header(64, 0, SHT_PROGBITS, 0, 0x10000);
        img.shown(128);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_section_that_stores_no_bytes_is_measured_by_its_offset_alone() {
        // `.bss` describes memory, not the file: its size says nothing about
        // how many bytes are there.
        let mut img = Image::elf64();
        img.shoff(64);
        img.shnum(1);
        img.section_header(64, 0, SHT_NOBITS_U32, 0, u64::MAX);
        img.shown(128);
        assert_eq!(check_elf_bounds(img.bytes()), Ok(()));
    }

    #[test]
    fn a_section_that_stores_no_bytes_still_needs_its_offset_inside_the_file() {
        // `get_shstr_table` slices `input[sh_offset..]` whatever the type says.
        let mut img = Image::elf64();
        img.shoff(64);
        img.shnum(1);
        img.section_header(64, 0, SHT_NOBITS_U32, 129, 0);
        img.shown(128);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    /// An image with three sections: the null one, the name table, and a
    /// `.text` of four bytes. `e_shstrndx` names the table.
    ///
    /// Names are `\0.shstrtab\0.text\0`, so `.shstrtab` is at offset 1 and
    /// `.text` at offset 11.
    fn three_sections() -> Image {
        let mut img = Image::elf64();
        img.shoff(64);
        img.shnum(3);
        img.shstrndx(1);
        img.section_header(64, 0, 0, 0, 0); // SHT_NULL
        img.section_header(128, 1, SHT_STRTAB, 256, 17);
        img.section_header(192, 11, SHT_PROGBITS, 273, 4);
        img.put(256, b"\0.shstrtab\0.text\0");
        img.put(273, b"\x90\x90\x90\xc3");
        img.shown(277);
        img
    }

    #[test]
    fn a_section_is_found_by_the_name_the_table_the_header_names_gives_it() {
        let img = three_sections();
        let elf = parse_checked_elf(img.bytes()).unwrap();
        let text = find_section(&elf, ".text").expect(".text");
        assert_eq!(text.offset(), 273);
        assert_eq!(section_bytes(&elf, &text), b"\x90\x90\x90\xc3");
        assert!(find_section(&elf, ".data").is_none());
        // A prefix of a real name is not that name.
        assert!(find_section(&elf, ".tex").is_none());
    }

    #[test]
    fn a_name_table_the_header_points_outside_the_file_leaves_every_name_empty() {
        let mut img = three_sections();
        img.shstrndx(3); // one past the last section
        let elf = parse_checked_elf(img.bytes()).unwrap();
        assert!(section_names(&elf).is_empty());
        assert!(find_section(&elf, ".text").is_none());
    }

    #[test]
    fn a_section_that_stores_no_bytes_hands_back_none_whatever_its_offset_says() {
        let mut img = three_sections();
        // Retype `.text` as `.bss`: its offset and size still say where four
        // bytes are, and there are none.
        img.section_header(192, 11, SHT_NOBITS_U32, 273, 4);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        let text = find_section(&elf, ".text").expect(".text");
        assert!(section_bytes(&elf, &text).is_empty());
    }

    #[test]
    fn a_section_count_above_the_reserved_indices_stops_where_the_parser_asserts() {
        let mut img = Image::elf64();
        img.shoff(64);
        img.shnum(0xffff);
        img.shown(256);
        // Not `parse_checked_elf`: a table of 65535 entries does not fit in
        // this file, and this bound is the one that holds for the files that
        // were never bounds-checked at all.
        assert_eq!(check_elf_bounds(img.bytes()), refused());
        let elf = ElfFile::new(img.bytes()).unwrap();
        assert_eq!(section_count(&elf), SHN_LORESERVE);
        assert_eq!(elf.header.pt2.sh_count(), 0xffff);
        // `elf.section_header(SHN_LORESERVE)` is the `assert!` this stands in
        // front of. What it does with an index BELOW the reserve and outside
        // the file is [`check_elf_bounds`]'s business, not this bound's.
        assert!(section_at(&elf, SHN_LORESERVE).is_none());
    }

    // ---- the address each header is read at --------------------------------

    #[test]
    fn a_program_header_table_at_an_odd_offset_is_refused() {
        let mut img = Image::elf64();
        img.phoff(65);
        img.phnum(1);
        img.shown(256);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_64_bit_program_header_table_four_bytes_off_is_refused() {
        // Four-byte alignment is enough for a 32-bit header and not for a
        // 64-bit one, so this is the offset that tells the two rules apart.
        let mut img = Image::elf64();
        img.phoff(68);
        img.phnum(1);
        img.shown(256);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_program_header_stride_that_leaves_the_second_entry_askew_is_refused() {
        // 57 is large enough for the struct the parser reads, so the size
        // check passes and entry 0 lands where it belongs; entry 1 does not.
        let mut img = Image::elf64();
        img.phoff(64);
        img.phentsize(57);
        img.phnum(2);
        img.shown(256);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
        // The same stride with a single entry has no second entry to misplace,
        // but the rule is per entry and entry 0 is where it should be.
        img.phnum(1);
        assert_eq!(check_elf_bounds(img.bytes()), Ok(()));
    }

    #[test]
    fn a_section_header_table_at_an_odd_offset_is_refused() {
        let mut img = Image::elf64();
        img.shoff(65);
        img.shnum(1);
        img.shown(256);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn a_section_header_stride_that_leaves_the_second_entry_askew_is_refused() {
        let mut img = Image::elf64();
        img.shoff(64);
        img.shentsize(65);
        img.shnum(2);
        img.shown(256);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    #[test]
    fn an_image_the_allocator_left_at_an_odd_address_is_refused() {
        // The image's own address anchors every header the parser reads, and
        // the header's second half is one of them.
        assert_eq!(check_elf_bounds(Image::elf64_at(1).bytes()), refused());
        assert_eq!(check_elf_bounds(Image::elf64_at(8).bytes()), Ok(()));
    }

    #[test]
    fn a_32_bit_image_needs_four_byte_alignment_and_no_more() {
        assert_eq!(check_elf_bounds(Image::elf32_at(4).bytes()), Ok(()));
        assert_eq!(check_elf_bounds(Image::elf32_at(2).bytes()), refused());
        // ... where a 64-bit image at the same address is refused.
        assert_eq!(check_elf_bounds(Image::elf64_at(4).bytes()), refused());
    }

    #[test]
    fn a_32_bit_table_four_bytes_off_is_accepted_where_a_64_bit_one_is_not() {
        let mut img = Image::elf32_at(0);
        img.phoff(56);
        img.phnum(1);
        img.shown(256);
        assert_eq!(check_elf_bounds(img.bytes()), Ok(()));
        img.phoff(54);
        assert_eq!(check_elf_bounds(img.bytes()), refused());
    }

    // ---- how many pages a segment takes ------------------------------------

    #[test]
    fn a_segment_spans_the_pages_from_its_first_page_to_its_last() {
        assert_eq!(segment_pages(0, 0), Ok(0));
        assert_eq!(segment_pages(1, 0), Ok(1));
        assert_eq!(segment_pages(PAGE_SIZE as u64, 0), Ok(1));
        assert_eq!(segment_pages(PAGE_SIZE as u64 + 1, 0), Ok(2));
        // The bytes below `virtual_addr` in its first page count too.
        assert_eq!(segment_pages(1, PAGE_SIZE - 1), Ok(1));
        assert_eq!(segment_pages(2, PAGE_SIZE - 1), Ok(2));
    }

    #[test]
    fn a_segment_bigger_than_the_address_space_is_refused() {
        assert_eq!(
            segment_pages(USER_ASPACE_SIZE + 1, 0),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            segment_pages(USER_ASPACE_SIZE, 1),
            Err(ZxError::INVALID_ARGS)
        );
    }

    #[test]
    fn the_largest_segment_the_address_space_holds_is_not_refused() {
        let pages = segment_pages(USER_ASPACE_SIZE, 0).unwrap();
        assert_eq!(pages, USER_ASPACE_SIZE as usize / PAGE_SIZE);
    }

    #[test]
    fn a_size_and_page_offset_that_wrap_are_refused() {
        assert_eq!(segment_pages(u64::MAX, 1), Err(ZxError::INVALID_ARGS));
    }

    // ---- how far an image reaches ------------------------------------------

    #[test]
    fn an_image_reaches_the_page_after_its_last_byte() {
        assert_eq!(segment_end_pages(0, 0), 0);
        assert_eq!(segment_end_pages(0, 1), 1);
        assert_eq!(segment_end_pages(PAGE_SIZE as u64, 0), 1);
        assert_eq!(segment_end_pages(PAGE_SIZE as u64, 1), 2);
    }

    #[test]
    fn an_end_that_does_not_fit_still_names_a_byte_count_that_does() {
        // The whole point of saturating here rather than in `pages()`: the
        // caller turns this back into bytes, and that must not overflow.
        let pages = segment_end_pages(u64::MAX, u64::MAX);
        assert_eq!(pages, usize::MAX / PAGE_SIZE);
        assert!(pages.checked_mul(PAGE_SIZE).is_some());
        assert_eq!(segment_end_pages(u64::MAX, 0), usize::MAX / PAGE_SIZE);
        // One byte over the top is the sum that wraps to nothing: an image
        // reported as zero pages long is an allocation that succeeds.
        assert_eq!(segment_end_pages(u64::MAX, 1), usize::MAX / PAGE_SIZE);
    }

    // ---- a name in a string table ------------------------------------------

    #[test]
    fn a_name_is_the_bytes_before_the_nul() {
        assert_eq!(str_at(b"\0.text\0.data\0", 1), Some(".text"));
        assert_eq!(str_at(b"\0.text\0.data\0", 7), Some(".data"));
    }

    #[test]
    fn an_offset_landing_on_the_nul_is_the_empty_name() {
        assert_eq!(str_at(b"\0.text\0", 0), Some(""));
    }

    #[test]
    fn a_table_with_no_nul_has_no_name() {
        // `zero::read_str` panics with "No null byte in input" here.
        assert_eq!(str_at(b".text", 0), None);
    }

    #[test]
    fn an_offset_past_the_end_of_the_table_has_no_name() {
        // The last byte of the table is its terminator, so the name that
        // starts there is the empty one; one byte further there is no table
        // left to scan, and past that there is no offset either.
        assert_eq!(str_at(b"\0.text\0", 6), Some(""));
        assert_eq!(str_at(b"\0.text\0", 7), None);
        assert_eq!(str_at(b"\0.text\0", 8), None);
        assert_eq!(str_at(b"", 0), None);
    }

    #[test]
    fn a_name_that_is_not_utf8_is_no_name() {
        // `zero::read_str` panics with "Non-utf8 string" here.
        assert_eq!(str_at(b"\xff\xfe\0", 0), None);
    }

    // ---- what the loader makes of a whole image ----------------------------

    #[test]
    fn an_interpreter_path_is_the_bytes_before_its_nul() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_INTERP, 0x80, 0, (16, 16));
        img.put(0x80, b"/lib/ld-musl.so\0");
        img.shown(0x90);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        assert_eq!(elf.get_interpreter(), Ok("/lib/ld-musl.so"));
    }

    #[test]
    fn an_interpreter_path_without_a_nul_is_refused() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_INTERP, 0x80, 0, (16, 16));
        img.put(0x80, b"/lib/ld-musl.so!");
        img.shown(0x90);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        assert!(elf.get_interpreter().is_err());
    }

    #[test]
    fn an_image_with_no_interpreter_has_none() {
        let img = one_load_segment(0x20_0000);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        assert!(elf.get_interpreter().is_err());
    }

    #[test]
    fn the_phdr_address_is_taken_from_the_phdr_segment_when_there_is_one() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_PHDR, 64, 0x40_0040, (56, 56));
        img.shown(256);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        assert_eq!(elf.get_phdr_vaddr(), Some(0x40_0040));
    }

    #[test]
    fn the_phdr_address_is_inferred_from_the_segment_at_the_start_of_the_file() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_LOAD, 0, 0x40_0000, (256, 256));
        img.shown(256);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        assert_eq!(elf.get_phdr_vaddr(), Some(0x40_0000 + 64));
    }

    #[test]
    fn an_inferred_phdr_address_that_does_not_fit_is_no_address() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_LOAD, 0, u64::MAX, (256, 256));
        img.shown(256);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        assert_eq!(elf.get_phdr_vaddr(), None);
    }

    #[test]
    fn an_image_with_no_segment_at_the_start_of_the_file_has_no_phdr_address() {
        let img = one_load_segment(0x20_0000);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        assert_eq!(elf.get_phdr_vaddr(), None);
    }

    #[test]
    fn the_image_size_is_the_end_of_its_highest_segment() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(2);
        img.program_header(64, PT_LOAD, 0, 0, (0x80, 0x80));
        img.program_header(120, PT_LOAD, 0x80, 0x2000, (16, 0x30));
        img.shown(256);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        assert_eq!(elf.load_segment_size(), 0x3000);
    }

    #[test]
    fn a_segment_that_ends_past_the_address_space_does_not_wrap_to_a_small_image() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_LOAD, 0, u64::MAX - 0xfff, (0, u64::MAX));
        img.shown(256);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        assert_eq!(elf.load_segment_size(), usize::MAX / PAGE_SIZE * PAGE_SIZE);
    }

    #[test]
    fn an_image_with_no_load_segment_maps_nothing() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_INTERP, 0x80, 0, (1, 1));
        img.put(0x80, b"\0");
        img.shown(0x90);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        let vmar = VmAddressRegion::new_root();
        assert_eq!(vmar.load_from_elf(&elf).err(), Some(ZxError::INVALID_ARGS));
    }

    #[test]
    fn a_segment_type_the_parser_does_not_know_is_skipped_rather_than_unwrapped() {
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(2);
        img.program_header(64, PT_UNKNOWN, 0xc0, 0x20_0000, (16, 16));
        img.program_header(120, PT_LOAD, 0xc0, 0x30_0000, (16, 16));
        img.put(0xc0, b"0123456789abcdef");
        img.shown(0xd0);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        let vmar = VmAddressRegion::new_root();
        let vmo = vmar.load_from_elf(&elf).unwrap();
        // The unknown segment was skipped, so the VMO handed back is the one
        // the `PT_LOAD` asked for -- and nothing was mapped at its address.
        assert_eq!(vmo.len(), PAGE_SIZE);
        assert!(vmar.find_mapping(vmar.addr() + 0x30_0000).is_some());
        assert!(vmar.find_mapping(vmar.addr() + 0x20_0000).is_none());
    }

    #[test]
    fn a_load_segment_is_mapped_at_its_own_page_with_its_bytes_in_place() {
        let img = one_load_segment(0x20_0040);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        let vmar = VmAddressRegion::new_root();
        let vmo = vmar.load_from_elf(&elf).unwrap();
        assert_eq!(vmo.len(), PAGE_SIZE);
        let mut buf = [0u8; 16];
        vmo.read(0x40, &mut buf).unwrap();
        assert_eq!(&buf, b"0123456789abcdef");
        // `p_vaddr` is an offset into the address space, which does not start
        // at zero when every process gets its own window out of the host's.
        assert!(vmar.find_mapping(vmar.addr() + 0x20_0000).is_some());
    }

    #[test]
    fn a_segment_with_more_bytes_in_the_file_than_in_memory_is_refused() {
        // `p_filesz > p_memsz` sizes the VMO for the memory image and then
        // writes the file image into it.
        let mut img = Image::elf64();
        img.phoff(64);
        img.phnum(1);
        img.program_header(64, PT_LOAD, 0x80, 0x20_0000, (16, 0));
        img.put(0x80, b"0123456789abcdef");
        img.shown(0x90);
        let elf = parse_checked_elf(img.bytes()).unwrap();
        let vmar = VmAddressRegion::new_root();
        assert!(vmar.load_from_elf(&elf).is_err());
    }
}
