//! This file is modified from 'page_table.rs' in 'rust-osdev/bootloader'
//!
//! The loader edits the firmware's own page tables, and those are the tables
//! the CPU is using: `main.rs` builds an `OffsetPageTable` over CR3 with a
//! zero offset, so physical memory is identity-mapped and a mapping is live
//! the instant `map_to` returns. That is what lets the `.bss` zeroing below
//! write through a kernel virtual address that did not exist a line earlier.
//!
//! **Nothing in here may panic.** By the time a mapping goes wrong the only
//! output left is a progress bar, so a `rboot.conf` with a typo in it or a
//! kernel ELF laid out a little differently than expected must come back as
//! an error, not as a machine that stops with a blank screen. Every address
//! that comes from a file is checked before it reaches `VirtAddr::new` or
//! `PhysAddr::new`, both of which panic on values a file can perfectly well
//! contain.

use core::fmt;
use log::warn;
use x86_64::structures::paging::{mapper::*, *};
use x86_64::{align_up, PhysAddr, VirtAddr};
use xmas_elf::{program, ElfFile};

/// The only page size the loader maps with.
pub const PAGE_SIZE: u64 = Size4KiB::SIZE;

/// Why a mapping could not be installed.
///
/// These replace what used to be a panic inside `x86_64` (`VirtAddr::new` on a
/// non-canonical address) or an `unwrap` on a malformed program header: the
/// same conditions, but named, so the failure reaches `main.rs` and says what
/// was wrong with which address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// Somebody else already owns this page and points it somewhere else.
    AlreadyMapped { page: u64, frame: u64 },
    /// The firmware would not give us another page.
    OutOfFrames,
    /// A 2 MiB or 1 GiB entry covers the address we needed 4 KiB granularity for.
    ParentEntryHugePage,
    /// Asked to unmap a page that is not mapped.
    NotMapped(u64),
    /// A page table held an entry whose address is not a valid frame address.
    InvalidFrame(u64),
    /// An address the CPU cannot express: bits 48..64 are not a sign extension
    /// of bit 47. `VirtAddr::new` panics on these; `rboot.conf` can hold one.
    NotCanonical(u64),
    /// A physical address with bits above 51 set. `PhysAddr::new` panics on these.
    NotPhysical(u64),
    /// An address range that runs off the end of the 64-bit address space.
    AddressOverflow(u64),
    /// `p_vaddr` and `p_offset` of a `PT_LOAD` disagree modulo the page size,
    /// which the ELF specification forbids precisely because a page-granular
    /// mapping cannot honour it: every byte of the segment would land
    /// `(vaddr - offset) mod 4096` bytes away from where the kernel was linked.
    MisalignedSegment { vaddr: u64, offset: u64 },
    /// A segment claims more bytes of file than the file has.
    SegmentOutsideFile {
        offset: u64,
        file_size: u64,
        len: u64,
    },
    /// `p_filesz > p_memsz`: more bytes to load than there is room for.
    FileLargerThanMemory { file_size: u64, mem_size: u64 },
    /// The kernel image was not loaded on a page boundary, so no page mapping
    /// can line its bytes up with the addresses it was linked for.
    UnalignedImage(u64),
    /// A kernel stack of zero pages: `stack_top()` would point at nothing.
    EmptyStack,
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyMapped { page, frame } => {
                write!(f, "page {page:#x} is already mapped, and not to {frame:#x}")
            }
            Self::OutOfFrames => write!(f, "the firmware ran out of pages"),
            Self::ParentEntryHugePage => write!(f, "a huge page covers this address"),
            Self::NotMapped(page) => write!(f, "page {page:#x} is not mapped"),
            Self::InvalidFrame(addr) => write!(f, "{addr:#x} is not a frame address"),
            Self::NotCanonical(addr) => write!(f, "{addr:#x} is not a canonical address"),
            Self::NotPhysical(addr) => write!(f, "{addr:#x} is not a physical address"),
            Self::AddressOverflow(addr) => write!(f, "the range at {addr:#x} wraps around"),
            Self::MisalignedSegment { vaddr, offset } => write!(
                f,
                "segment at vaddr {vaddr:#x} / file offset {offset:#x} is not page congruent"
            ),
            Self::SegmentOutsideFile {
                offset,
                file_size,
                len,
            } => write!(
                f,
                "segment wants {file_size:#x} bytes at {offset:#x} of a {len:#x}-byte file"
            ),
            Self::FileLargerThanMemory {
                file_size,
                mem_size,
            } => write!(f, "p_filesz {file_size:#x} > p_memsz {mem_size:#x}"),
            Self::UnalignedImage(addr) => {
                write!(f, "the kernel image is loaded at {addr:#x}, not on a page")
            }
            Self::EmptyStack => write!(f, "a kernel stack of zero pages"),
        }
    }
}

impl From<MapToError<Size4KiB>> for MapError {
    fn from(e: MapToError<Size4KiB>) -> Self {
        match e {
            MapToError::FrameAllocationFailed => Self::OutOfFrames,
            MapToError::ParentEntryHugePage => Self::ParentEntryHugePage,
            MapToError::PageAlreadyMapped(frame) => Self::AlreadyMapped {
                page: 0,
                frame: frame.start_address().as_u64(),
            },
        }
    }
}

impl From<UnmapError> for MapError {
    fn from(e: UnmapError) -> Self {
        match e {
            UnmapError::ParentEntryHugePage => Self::ParentEntryHugePage,
            UnmapError::PageNotMapped => Self::NotMapped(0),
            UnmapError::InvalidFrameAddress(addr) => Self::InvalidFrame(addr.as_u64()),
        }
    }
}

/// Everything the loader asks of the machine while it lays the kernel out.
///
/// Every one of these is either ring-0 only or a raw pointer write: the TLB
/// flush inside `MapperFlush` is an `invlpg`, which is a #GP outside the
/// kernel, and the `.bss` zeroing writes through an address that only means
/// something with the loader's page tables installed. Behind a trait, the
/// arithmetic in this module -- which is where its bugs live -- runs on an
/// ordinary host.
pub trait Machine {
    /// A fresh, zero-or-garbage-filled 4 KiB frame, or `None` when the
    /// firmware has no more.
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>>;

    /// Install `page -> frame`, allocating any intermediate tables, and make
    /// the CPU see it.
    ///
    /// # Safety
    /// The caller promises the page is not aliased in a way that matters.
    unsafe fn map(
        &mut self,
        page: Page<Size4KiB>,
        frame: PhysFrame<Size4KiB>,
        flags: PageTableFlags,
    ) -> Result<(), MapError>;

    /// Take `page` out of the tables.
    ///
    /// # Safety
    /// The caller promises nothing is about to read through `page`.
    unsafe fn unmap(&mut self, page: Page<Size4KiB>) -> Result<(), MapError>;

    /// Where `page` currently points, if anywhere.
    fn translate(&self, page: Page<Size4KiB>) -> Option<PhysFrame<Size4KiB>>;

    /// Write `len` zero bytes at virtual address `at`.
    ///
    /// # Safety
    /// `at .. at + len` must be mapped and writable.
    unsafe fn zero(&mut self, at: u64, len: usize);

    /// Copy a whole page from `src` to `dst`, both physical (identity-mapped)
    /// addresses.
    ///
    /// # Safety
    /// Both must be mapped, page-aligned and 4 KiB long.
    unsafe fn copy_page(&mut self, dst: u64, src: u64);
}

/// The real machine: the firmware's page tables and its page allocator.
pub struct Firmware<'a, M, A> {
    pub mapper: &'a mut M,
    pub allocator: &'a mut A,
}

impl<M, A> Machine for Firmware<'_, M, A>
where
    M: Mapper<Size4KiB>,
    A: FrameAllocator<Size4KiB>,
{
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        self.allocator.allocate_frame()
    }

    unsafe fn map(
        &mut self,
        page: Page<Size4KiB>,
        frame: PhysFrame<Size4KiB>,
        flags: PageTableFlags,
    ) -> Result<(), MapError> {
        match self.mapper.map_to(page, frame, flags, self.allocator) {
            Ok(flush) => {
                flush.flush();
                Ok(())
            }
            Err(MapToError::PageAlreadyMapped(existing)) => Err(MapError::AlreadyMapped {
                page: page.start_address().as_u64(),
                frame: existing.start_address().as_u64(),
            }),
            Err(e) => Err(e.into()),
        }
    }

    unsafe fn unmap(&mut self, page: Page<Size4KiB>) -> Result<(), MapError> {
        match self.mapper.unmap(page) {
            Ok((_, flush)) => {
                flush.flush();
                Ok(())
            }
            Err(UnmapError::PageNotMapped) => {
                Err(MapError::NotMapped(page.start_address().as_u64()))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn translate(&self, page: Page<Size4KiB>) -> Option<PhysFrame<Size4KiB>> {
        self.mapper.translate_page(page).ok()
    }

    unsafe fn zero(&mut self, at: u64, len: usize) {
        core::ptr::write_bytes(at as *mut u8, 0, len);
    }

    unsafe fn copy_page(&mut self, dst: u64, src: u64) {
        type PageArray = [u64; PAGE_SIZE as usize / 8];
        (dst as *mut PageArray).write((src as *const PageArray).read());
    }
}

/// The page holding `addr`, or an error if `addr` is not an address the CPU
/// can form. `VirtAddr::new` panics there, and `addr` comes from a file.
fn page_of(addr: u64) -> Result<Page<Size4KiB>, MapError> {
    VirtAddr::try_new(addr)
        .map(Page::containing_address)
        .map_err(|_| MapError::NotCanonical(addr))
}

/// The frame holding `addr`, or an error if `addr` has bits above 51 set.
fn frame_of(addr: u64) -> Result<PhysFrame<Size4KiB>, MapError> {
    PhysAddr::try_new(addr)
        .map(PhysFrame::containing_address)
        .map_err(|_| MapError::NotPhysical(addr))
}

/// The frame holding byte `off` of the kernel image.
fn image_frame(kernel_start: u64, off: u64) -> Result<PhysFrame<Size4KiB>, MapError> {
    frame_of(
        kernel_start
            .checked_add(off)
            .ok_or(MapError::AddressOverflow(kernel_start))?,
    )
}

/// Install one mapping, accepting a page the firmware (or an earlier segment)
/// already pointed at the very same frame.
///
/// Both callers used to answer this question, and answered it differently:
/// `map_segment` returned an error and `map_physical_memory` panicked. One
/// answer now.
fn map_page(
    m: &mut impl Machine,
    page: Page<Size4KiB>,
    frame: PhysFrame<Size4KiB>,
    flags: PageTableFlags,
) -> Result<(), MapError> {
    match unsafe { m.map(page, frame, flags) } {
        Err(MapError::AlreadyMapped { .. }) if m.translate(page) == Some(frame) => Ok(()),
        other => other,
    }
}

/// Map every `PT_LOAD` of `elf` where it was linked to run.
pub fn map_elf(elf: &ElfFile, m: &mut impl Machine) -> Result<(), MapError> {
    let kernel_start = elf.input.as_ptr() as u64;
    // Every address below is `kernel_start + file offset`, rounded down to a
    // page and handed to the MMU. That only lines the bytes up with the
    // addresses the kernel was linked for if the image itself starts on a
    // page; `load_file` allocates pages, so it does, but nothing said so.
    if !kernel_start.is_multiple_of(PAGE_SIZE) {
        return Err(MapError::UnalignedImage(kernel_start));
    }
    map_elf_at(elf, kernel_start, m)
}

/// `map_elf`, told where the image sits rather than reading it off the slice.
pub fn map_elf_at(elf: &ElfFile, kernel_start: u64, m: &mut impl Machine) -> Result<(), MapError> {
    let len = elf.input.len() as u64;
    for segment in elf.program_iter() {
        map_segment(&segment, kernel_start, len, m)?;
    }
    Ok(())
}

/// Map the kernel stack: `pages` 4 KiB pages covering `addr`, up to and
/// including the byte below `addr + pages * 4096`, which is what
/// `Config::stack_top()` hands the kernel as its initial `rsp`.
pub fn map_stack(addr: u64, pages: u64, m: &mut impl Machine) -> Result<(), MapError> {
    if pages == 0 {
        return Err(MapError::EmptyStack);
    }
    let bytes = pages
        .checked_mul(PAGE_SIZE)
        .ok_or(MapError::AddressOverflow(addr))?;
    let last = addr
        .checked_add(bytes - 1)
        .ok_or(MapError::AddressOverflow(addr))?;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    for page in Page::range_inclusive(page_of(addr)?, page_of(last)?) {
        let frame = m.allocate_frame().ok_or(MapError::OutOfFrames)?;
        unsafe { m.map(page, frame, flags)? };
    }
    Ok(())
}

/// Map physical memory `[0, max_addr]`, rounded out to whole frames, to
/// virtual space at `offset`.
///
/// `max_addr` is included on purpose: every caller computes it as "one past
/// the last byte the kernel will touch" (`fb_addr + fb_size`,
/// `initramfs_addr + initramfs_size`), and a framebuffer whose size is not a
/// multiple of 4 KiB would otherwise lose its last page.
pub fn map_physical_memory(
    offset: u64,
    max_addr: u64,
    m: &mut impl Machine,
) -> Result<(), MapError> {
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    for frame in PhysFrame::range_inclusive(frame_of(0)?, frame_of(max_addr)?) {
        let phys = frame.start_address().as_u64();
        let virt = offset
            .checked_add(phys)
            .ok_or(MapError::AddressOverflow(offset))?;
        map_page(m, page_of(virt)?, frame, flags)?;
    }
    Ok(())
}

fn map_segment(
    segment: &program::ProgramHeader,
    kernel_start: u64,
    kernel_len: u64,
    m: &mut impl Machine,
) -> Result<(), MapError> {
    match segment.get_type() {
        Ok(program::Type::Load) => {}
        Ok(_) => return Ok(()),
        Err(e) => {
            // A program header type outside every range xmas-elf knows. Used
            // to be `.unwrap()`, i.e. a kernel that a slightly unusual linker
            // emitted was a machine that stopped with no message.
            warn!("skipping a program header the loader does not know: {}", e);
            return Ok(());
        }
    }

    let mem_size = segment.mem_size();
    let file_size = segment.file_size();
    let offset = segment.offset();
    let vaddr = segment.virtual_addr();

    if mem_size == 0 {
        return Ok(());
    }
    if file_size > mem_size {
        return Err(MapError::FileLargerThanMemory {
            file_size,
            mem_size,
        });
    }
    // The ELF specification requires `p_vaddr ≡ p_offset (mod page size)`.
    // Nothing checked it, and the mapping below is page-granular: a segment
    // that breaks the rule is loaded silently, every byte of it displaced.
    if !(vaddr ^ offset).is_multiple_of(PAGE_SIZE) {
        return Err(MapError::MisalignedSegment { vaddr, offset });
    }
    if offset
        .checked_add(file_size)
        .is_none_or(|end| end > kernel_len)
    {
        return Err(MapError::SegmentOutsideFile {
            offset,
            file_size,
            len: kernel_len,
        });
    }
    // Both ends of the virtual range, so `page_of` has rejected anything
    // non-canonical before a `VirtAddr` is built from it below.
    let virt_end = vaddr
        .checked_add(mem_size - 1)
        .ok_or(MapError::AddressOverflow(vaddr))?;
    page_of(vaddr)?;
    page_of(virt_end)?;

    // Don't force NX on loader-created kernel mappings. Some real-world kernel
    // ELF layouts/flags can make this deny the very first instruction fetch,
    // resulting in "100% progress but no logs/shell". The kernel installs its
    // own page tables/policy right after handoff.
    let mut flags = PageTableFlags::PRESENT;
    if segment.flags().is_write() {
        flags |= PageTableFlags::WRITABLE;
    }

    if file_size != 0 {
        let start_page = page_of(vaddr)?;
        let start_frame = image_frame(kernel_start, offset & !(PAGE_SIZE - 1))?;
        // `file_size` is counted from `offset`, not from the page `offset`
        // starts in, so the last byte of the segment is at
        // `kernel_start + offset + file_size - 1`. Measuring it from the
        // rounded-down base instead left the last page of every segment
        // whose file offset was not page-aligned -- which is most of them
        // after the first -- out of the page tables.
        let end_frame = image_frame(kernel_start, offset + file_size - 1)?;
        for frame in PhysFrame::range_inclusive(start_frame, end_frame) {
            let page = start_page + (frame - start_frame);
            map_page(m, page, frame, flags)?;
        }
    }

    if mem_size > file_size {
        let zero_start = vaddr + file_size;
        let zero_end = vaddr + mem_size; // exclusive

        if file_size != 0 && !zero_start.is_multiple_of(PAGE_SIZE) {
            // The last file-backed page holds `.bss` bytes too, and it is
            // still shared with whatever follows it in the kernel image.
            // Copy it onto a private frame before anything zeroes into it.
            let new_frame = m.allocate_frame().ok_or(MapError::OutOfFrames)?;
            let last_page = page_of(zero_start - 1)?;
            let last_frame = image_frame(kernel_start, offset + file_size - 1)?;
            unsafe {
                m.copy_page(
                    new_frame.start_address().as_u64(),
                    last_frame.start_address().as_u64(),
                );
                m.unmap(last_page)?;
                m.map(last_page, new_frame, flags)?;
            }
        }

        // The first page no file-backed frame already covers. With no file
        // bytes at all there is none, and rounding up would have left the
        // head of a `.bss`-only segment unmapped and then zeroed into it.
        let first = if file_size == 0 {
            zero_start
        } else {
            align_up(zero_start, PAGE_SIZE)
        };
        // `zero_end` is one past the end: taking the page containing it
        // mapped a spare page beyond the segment, which is a page the next
        // segment then found already mapped, to the wrong frame.
        let last = zero_end - 1;
        if first <= last {
            for page in Page::range_inclusive(page_of(first)?, page_of(last)?) {
                let frame = m.allocate_frame().ok_or(MapError::OutOfFrames)?;
                map_page(m, page, frame, flags)?;
            }
        }

        unsafe { m.zero(zero_start, (mem_size - file_size) as usize) };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    /// Where the tests pretend the kernel image was loaded. Page-aligned, as
    /// `load_file`'s `allocate_pages` guarantees on the real machine.
    const IMAGE: u64 = 0x1_0000_0000;
    /// Where the fake firmware hands out fresh frames from.
    const FRESH: u64 = 0x2_0000_0000;
    /// A canonical higher-half base, the shape zCore is linked at.
    const KVA: u64 = 0xffff_ff00_0000_0000;

    const PT_LOAD: u32 = 1;
    const PT_NOTE: u32 = 4;
    const PF_X: u32 = 1;
    const PF_W: u32 = 2;
    const PF_R: u32 = 4;

    // ---------------------------------------------------------------- machine

    #[derive(Default)]
    struct Fake {
        mapped: BTreeMap<u64, (u64, PageTableFlags)>,
        handed: u64,
        budget: Option<u64>,
        zeroed: Vec<(u64, usize)>,
        copied: Vec<(u64, u64)>,
        unmapped: Vec<u64>,
    }

    impl Fake {
        fn with_frames(n: u64) -> Self {
            Fake {
                budget: Some(n),
                ..Default::default()
            }
        }
        /// Pre-place a mapping, the way the firmware leaves some behind.
        fn preset(&mut self, virt: u64, phys: u64) {
            self.mapped.insert(
                virt,
                (phys, PageTableFlags::PRESENT | PageTableFlags::WRITABLE),
            );
        }
        fn frame_at(&self, virt: u64) -> Option<u64> {
            self.mapped.get(&virt).map(|(f, _)| *f)
        }
        fn flags_at(&self, virt: u64) -> Option<PageTableFlags> {
            self.mapped.get(&virt).map(|(_, f)| *f)
        }
        fn pages(&self) -> Vec<u64> {
            self.mapped.keys().copied().collect()
        }
    }

    impl Machine for Fake {
        fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
            if let Some(left) = self.budget {
                if self.handed >= left {
                    return None;
                }
            }
            let addr = FRESH + self.handed * PAGE_SIZE;
            self.handed += 1;
            Some(PhysFrame::containing_address(PhysAddr::new(addr)))
        }

        unsafe fn map(
            &mut self,
            page: Page<Size4KiB>,
            frame: PhysFrame<Size4KiB>,
            flags: PageTableFlags,
        ) -> Result<(), MapError> {
            let virt = page.start_address().as_u64();
            if let Some((existing, _)) = self.mapped.get(&virt) {
                return Err(MapError::AlreadyMapped {
                    page: virt,
                    frame: *existing,
                });
            }
            self.mapped
                .insert(virt, (frame.start_address().as_u64(), flags));
            Ok(())
        }

        unsafe fn unmap(&mut self, page: Page<Size4KiB>) -> Result<(), MapError> {
            let virt = page.start_address().as_u64();
            match self.mapped.remove(&virt) {
                Some(_) => {
                    self.unmapped.push(virt);
                    Ok(())
                }
                None => Err(MapError::NotMapped(virt)),
            }
        }

        fn translate(&self, page: Page<Size4KiB>) -> Option<PhysFrame<Size4KiB>> {
            self.frame_at(page.start_address().as_u64())
                .map(|f| PhysFrame::containing_address(PhysAddr::new(f)))
        }

        unsafe fn zero(&mut self, at: u64, len: usize) {
            self.zeroed.push((at, len));
        }

        unsafe fn copy_page(&mut self, dst: u64, src: u64) {
            self.copied.push((dst, src));
        }
    }

    // -------------------------------------------------------------- elf files

    #[derive(Clone, Copy)]
    struct Seg {
        typ: u32,
        flags: u32,
        offset: u64,
        vaddr: u64,
        file_size: u64,
        mem_size: u64,
    }

    /// A read-only `PT_LOAD` whose file and memory sizes agree.
    fn load(offset: u64, vaddr: u64, size: u64) -> Seg {
        Seg {
            typ: PT_LOAD,
            flags: PF_R,
            offset,
            vaddr,
            file_size: size,
            mem_size: size,
        }
    }

    /// A little ELF64 with the program headers the test cares about and
    /// nothing else. `len` pads the file out so the bounds check has
    /// something to bound against.
    fn elf_bytes(segs: &[Seg], len: usize) -> Vec<u8> {
        let mut v = std::vec![0u8; 64 + segs.len() * 56];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = 2; // ELFCLASS64
        v[5] = 1; // little endian
        v[6] = 1; // EI_VERSION
        v[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        v[18..20].copy_from_slice(&0x3eu16.to_le_bytes()); // EM_X86_64
        v[20..24].copy_from_slice(&1u32.to_le_bytes());
        v[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        v[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
        v[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        v[56..58].copy_from_slice(&(segs.len() as u16).to_le_bytes());
        v[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
        for (i, s) in segs.iter().enumerate() {
            let o = 64 + i * 56;
            v[o..o + 4].copy_from_slice(&s.typ.to_le_bytes());
            v[o + 4..o + 8].copy_from_slice(&s.flags.to_le_bytes());
            v[o + 8..o + 16].copy_from_slice(&s.offset.to_le_bytes());
            v[o + 16..o + 24].copy_from_slice(&s.vaddr.to_le_bytes());
            v[o + 24..o + 32].copy_from_slice(&s.vaddr.to_le_bytes());
            v[o + 32..o + 40].copy_from_slice(&s.file_size.to_le_bytes());
            v[o + 40..o + 48].copy_from_slice(&s.mem_size.to_le_bytes());
            v[o + 48..o + 56].copy_from_slice(&PAGE_SIZE.to_le_bytes());
        }
        v.resize(len.max(v.len()), 0);
        v
    }

    /// Lay the segments out and map them at `IMAGE`, returning the machine.
    fn map_segs(segs: &[Seg], len: usize) -> (Fake, Result<(), MapError>) {
        let bytes = elf_bytes(segs, len);
        let elf = ElfFile::new(&bytes).expect("test ELF is not an ELF");
        let mut m = Fake::default();
        let r = map_elf_at(&elf, IMAGE, &mut m);
        (m, r)
    }

    fn mapped_ok(segs: &[Seg], len: usize) -> Fake {
        let (m, r) = map_segs(segs, len);
        r.expect("mapping the segments failed");
        m
    }

    /// The same bytes, placed at a known distance from a page boundary, so
    /// `map_elf`'s view of where the image starts can be steered.
    struct Placed {
        buf: Vec<u8>,
        at: usize,
        len: usize,
    }

    impl Placed {
        fn new(bytes: &[u8], past_page: usize) -> Placed {
            let mut buf = std::vec![0u8; bytes.len() + 2 * PAGE_SIZE as usize];
            let base = buf.as_ptr() as usize;
            let pad = (PAGE_SIZE as usize - base % PAGE_SIZE as usize) % PAGE_SIZE as usize;
            let at = pad + past_page;
            buf[at..at + bytes.len()].copy_from_slice(bytes);
            Placed {
                buf,
                at,
                len: bytes.len(),
            }
        }
        fn slice(&self) -> &[u8] {
            &self.buf[self.at..self.at + self.len]
        }
    }

    // ------------------------------------------------------- the file-backed part

    #[test]
    fn a_page_aligned_segment_maps_exactly_the_frames_it_occupies() {
        let m = mapped_ok(&[load(0x1000, KVA + 0x1000, 0x2800)], 0x8000);
        assert_eq!(
            m.pages(),
            std::vec![KVA + 0x1000, KVA + 0x2000, KVA + 0x3000]
        );
        for i in 1..=3u64 {
            assert_eq!(
                m.frame_at(KVA + i * PAGE_SIZE),
                Some(IMAGE + i * PAGE_SIZE),
                "page {i} points at the wrong frame"
            );
        }
    }

    #[test]
    fn the_last_page_of_a_segment_whose_file_offset_is_not_page_aligned_is_mapped() {
        // 0x2abc + 0x1600 - 1 = 0x40bb, so the segment's last byte lives in
        // the frame at 0x4000. Measuring the extent from the rounded-down
        // 0x2000 instead stopped one whole page short.
        let m = mapped_ok(&[load(0x2abc, KVA + 0x2abc, 0x1600)], 0x8000);
        assert_eq!(
            m.pages(),
            std::vec![KVA + 0x2000, KVA + 0x3000, KVA + 0x4000]
        );
        assert_eq!(m.frame_at(KVA + 0x4000), Some(IMAGE + 0x4000));
    }

    #[test]
    fn every_byte_of_a_segment_lands_on_a_mapped_page() {
        for offset in [0x1000u64, 0x1001, 0x1abc, 0x1fff] {
            for size in [1u64, 0xfff, 0x1000, 0x1001, 0x2345] {
                let m = mapped_ok(&[load(offset, KVA + offset, size)], 0x8000);
                for byte in [0u64, size / 2, size - 1] {
                    let page = (KVA + offset + byte) & !(PAGE_SIZE - 1);
                    assert!(
                        m.frame_at(page).is_some(),
                        "offset {offset:#x} size {size:#x}: byte {byte:#x} is unmapped"
                    );
                }
            }
        }
    }

    #[test]
    fn a_writable_segment_is_mapped_writable_and_a_read_only_one_is_not() {
        let mut text = load(0x1000, KVA + 0x1000, 0x1000);
        text.flags = PF_R | PF_X;
        let mut data = load(0x2000, KVA + 0x2000, 0x1000);
        data.flags = PF_R | PF_W;
        let m = mapped_ok(&[text, data], 0x8000);
        assert_eq!(m.flags_at(KVA + 0x1000), Some(PageTableFlags::PRESENT));
        assert_eq!(
            m.flags_at(KVA + 0x2000),
            Some(PageTableFlags::PRESENT | PageTableFlags::WRITABLE)
        );
    }

    #[test]
    fn segments_that_are_not_pt_load_are_skipped() {
        let mut note = load(0x1000, KVA + 0x1000, 0x1000);
        note.typ = PT_NOTE;
        let m = mapped_ok(&[note], 0x8000);
        assert!(m.pages().is_empty());
    }

    #[test]
    fn a_program_header_type_the_loader_does_not_know_is_skipped_instead_of_panicking() {
        // 8 is in no range xmas-elf knows, so `get_type()` is an `Err`, and
        // the loader used to `.unwrap()` it: a kernel from a slightly
        // unusual linker was a machine that stopped without a word.
        let mut odd = load(0x1000, KVA + 0x1000, 0x1000);
        odd.typ = 8;
        let (m, r) = map_segs(&[odd], 0x8000);
        assert_eq!(r, Ok(()));
        assert!(m.pages().is_empty());
    }

    #[test]
    fn a_segment_whose_vaddr_and_offset_disagree_modulo_a_page_is_refused() {
        let (_, r) = map_segs(&[load(0x1000, KVA + 0x1234, 0x1000)], 0x8000);
        assert_eq!(
            r,
            Err(MapError::MisalignedSegment {
                vaddr: KVA + 0x1234,
                offset: 0x1000
            })
        );
    }

    #[test]
    fn a_segment_that_runs_off_the_end_of_the_file_is_refused() {
        let (_, r) = map_segs(&[load(0x1000, KVA + 0x1000, 0x9000)], 0x8000);
        assert_eq!(
            r,
            Err(MapError::SegmentOutsideFile {
                offset: 0x1000,
                file_size: 0x9000,
                len: 0x8000
            })
        );
    }

    #[test]
    fn a_segment_whose_offset_and_size_wrap_around_is_refused() {
        let (_, r) = map_segs(&[load(u64::MAX - 0xfff, KVA, 0x2000)], 0x8000);
        assert!(matches!(r, Err(MapError::SegmentOutsideFile { .. })));
    }

    #[test]
    fn a_non_canonical_vaddr_is_refused_instead_of_panicking() {
        // Bit 47 clear, bit 48 set: `VirtAddr::new` panics on this, and the
        // vaddr comes straight out of the kernel file.
        let bad = 0x0001_0000_0000_1000;
        let (_, r) = map_segs(&[load(0x1000, bad, 0x1000)], 0x8000);
        assert_eq!(r, Err(MapError::NotCanonical(bad)));
    }

    #[test]
    fn p_filesz_larger_than_p_memsz_is_refused() {
        let mut s = load(0x1000, KVA + 0x1000, 0x2000);
        s.mem_size = 0x1000;
        let (_, r) = map_segs(&[s], 0x8000);
        assert_eq!(
            r,
            Err(MapError::FileLargerThanMemory {
                file_size: 0x2000,
                mem_size: 0x1000
            })
        );
    }

    #[test]
    fn two_segments_sharing_a_page_share_its_frame_instead_of_colliding() {
        let a = load(0x1000, KVA + 0x1000, 0x500);
        let b = load(0x1500, KVA + 0x1500, 0x300);
        let m = mapped_ok(&[a, b], 0x8000);
        assert_eq!(m.pages(), std::vec![KVA + 0x1000]);
        assert_eq!(m.frame_at(KVA + 0x1000), Some(IMAGE + 0x1000));
    }

    #[test]
    fn a_page_somebody_else_points_elsewhere_is_an_error_not_a_silent_kernel() {
        let bytes = elf_bytes(&[load(0x1000, KVA + 0x1000, 0x1000)], 0x8000);
        let elf = ElfFile::new(&bytes).unwrap();
        let mut m = Fake::default();
        m.preset(KVA + 0x1000, 0xdead_0000);
        assert_eq!(
            map_elf_at(&elf, IMAGE, &mut m),
            Err(MapError::AlreadyMapped {
                page: KVA + 0x1000,
                frame: 0xdead_0000
            })
        );
    }

    // -------------------------------------------------------------- the bss part

    #[test]
    fn a_bss_only_segment_that_does_not_start_on_a_page_still_gets_its_head_mapped() {
        // With no file bytes there is no file-backed page to lean on, and
        // rounding the first bss page up walked straight past the only page
        // the segment has -- which the zeroing below then wrote into.
        let s = Seg {
            typ: PT_LOAD,
            flags: PF_R | PF_W,
            offset: 0x1abc,
            vaddr: KVA + 0x1abc,
            file_size: 0,
            mem_size: 0x100,
        };
        let m = mapped_ok(&[s], 0x8000);
        assert_eq!(m.pages(), std::vec![KVA + 0x1000]);
        assert_eq!(m.zeroed, std::vec![(KVA + 0x1abc, 0x100)]);
        assert!(m.copied.is_empty(), "nothing to copy without file bytes");
    }

    #[test]
    fn the_bss_does_not_claim_a_spare_page_past_its_end() {
        let s = Seg {
            typ: PT_LOAD,
            flags: PF_R | PF_W,
            offset: 0x1000,
            vaddr: KVA + 0x1000,
            file_size: 0x1000,
            mem_size: 0x2000,
        };
        let m = mapped_ok(&[s], 0x8000);
        assert_eq!(m.pages(), std::vec![KVA + 0x1000, KVA + 0x2000]);
        assert_eq!(m.frame_at(KVA + 0x3000), None);
    }

    #[test]
    fn the_spare_page_past_a_bss_no_longer_collides_with_the_next_segment() {
        let bss = Seg {
            typ: PT_LOAD,
            flags: PF_R | PF_W,
            offset: 0x1000,
            vaddr: KVA + 0x1000,
            file_size: 0x1000,
            mem_size: 0x2000,
        };
        let next = load(0x3000, KVA + 0x3000, 0x1000);
        let m = mapped_ok(&[bss, next], 0x8000);
        assert_eq!(m.frame_at(KVA + 0x3000), Some(IMAGE + 0x3000));
    }

    #[test]
    fn a_partial_tail_page_is_copied_onto_a_private_frame_before_it_is_zeroed() {
        let s = Seg {
            typ: PT_LOAD,
            flags: PF_R | PF_W,
            offset: 0x1000,
            vaddr: KVA + 0x1000,
            file_size: 0x1800,
            mem_size: 0x2000,
        };
        let m = mapped_ok(&[s], 0x8000);
        assert_eq!(m.copied, std::vec![(FRESH, IMAGE + 0x2000)]);
        assert_eq!(m.unmapped, std::vec![KVA + 0x2000]);
        assert_eq!(m.frame_at(KVA + 0x2000), Some(FRESH));
        assert_eq!(m.frame_at(KVA + 0x1000), Some(IMAGE + 0x1000));
        assert_eq!(m.zeroed, std::vec![(KVA + 0x2800, 0x800)]);
    }

    #[test]
    fn the_tail_page_copied_for_a_bss_comes_from_where_the_file_bytes_really_are() {
        // Same off-by-a-page as the mapping loop: the source frame was taken
        // from the rounded-down file offset, so a segment that did not start
        // on a page had the wrong page of the kernel copied under its tail.
        let s = Seg {
            typ: PT_LOAD,
            flags: PF_R | PF_W,
            offset: 0x2abc,
            vaddr: KVA + 0x2abc,
            file_size: 0x1600,
            mem_size: 0x1700,
        };
        let m = mapped_ok(&[s], 0x8000);
        assert_eq!(m.copied, std::vec![(FRESH, IMAGE + 0x4000)]);
        assert_eq!(m.frame_at(KVA + 0x4000), Some(FRESH));
    }

    #[test]
    fn the_bss_is_zeroed_from_the_last_file_byte_to_the_end_of_the_segment() {
        let s = Seg {
            typ: PT_LOAD,
            flags: PF_R | PF_W,
            offset: 0x1000,
            vaddr: KVA + 0x1000,
            file_size: 0x40,
            mem_size: 0x2_0000,
        };
        let m = mapped_ok(&[s], 0x8000);
        assert_eq!(m.zeroed, std::vec![(KVA + 0x1040, 0x2_0000 - 0x40)]);
    }

    #[test]
    fn a_segment_with_no_memory_at_all_maps_nothing() {
        let m = mapped_ok(&[load(0x1000, KVA + 0x1000, 0)], 0x8000);
        assert!(m.pages().is_empty());
        assert!(m.zeroed.is_empty());
    }

    #[test]
    fn running_out_of_frames_in_the_bss_is_an_error_not_a_hole() {
        let bytes = elf_bytes(
            &[Seg {
                typ: PT_LOAD,
                flags: PF_R | PF_W,
                offset: 0x1000,
                vaddr: KVA + 0x1000,
                file_size: 0x1000,
                mem_size: 0x8000,
            }],
            0x8000,
        );
        let elf = ElfFile::new(&bytes).unwrap();
        let mut m = Fake::with_frames(2);
        assert_eq!(map_elf_at(&elf, IMAGE, &mut m), Err(MapError::OutOfFrames));
    }

    #[test]
    fn a_three_segment_kernel_maps_a_gap_free_range() {
        let mut text = load(0x1000, KVA + 0x1000, 0x3000);
        text.flags = PF_R | PF_X;
        let rodata = load(0x4000, KVA + 0x4000, 0x1800);
        let data = Seg {
            typ: PT_LOAD,
            flags: PF_R | PF_W,
            offset: 0x6000,
            vaddr: KVA + 0x6000,
            file_size: 0x800,
            mem_size: 0x2000,
        };
        let m = mapped_ok(&[text, rodata, data], 0x8000);
        let want: Vec<u64> = (1..=7).map(|i| KVA + i * PAGE_SIZE).collect();
        assert_eq!(m.pages(), want);
        for i in 1..=5u64 {
            assert_eq!(
                m.flags_at(KVA + i * PAGE_SIZE),
                Some(PageTableFlags::PRESENT),
                "page {i} of a read-only segment is writable"
            );
        }
        for i in 6..=7u64 {
            assert!(m
                .flags_at(KVA + i * PAGE_SIZE)
                .unwrap()
                .contains(PageTableFlags::WRITABLE));
        }
    }

    // ------------------------------------------------------------- the image base

    #[test]
    fn map_elf_reads_the_image_base_off_the_slice() {
        let bytes = elf_bytes(&[load(0x1000, KVA + 0x1000, 0x1000)], 0x8000);
        let placed = Placed::new(&bytes, 0);
        let elf = ElfFile::new(placed.slice()).unwrap();
        let base = placed.slice().as_ptr() as u64;
        let mut m = Fake::default();
        assert_eq!(map_elf(&elf, &mut m), Ok(()));
        assert_eq!(m.frame_at(KVA + 0x1000), Some(base + 0x1000));
    }

    #[test]
    fn an_image_that_does_not_start_on_a_page_is_refused() {
        // Every address below is `image + file offset` rounded down to a
        // page: if the image itself is off the grid, every byte of the
        // kernel lands displaced and nothing says so.
        let bytes = elf_bytes(&[load(0x1000, KVA + 0x1000, 0x1000)], 0x8000);
        let placed = Placed::new(&bytes, 0x800);
        let elf = ElfFile::new(placed.slice()).unwrap();
        let base = placed.slice().as_ptr() as u64;
        let mut m = Fake::default();
        assert_eq!(map_elf(&elf, &mut m), Err(MapError::UnalignedImage(base)));
    }

    // -------------------------------------------------------------- the stack

    #[test]
    fn the_stack_covers_the_top_the_config_hands_the_kernel() {
        // `map_stack` and `Config::stack_top()` live in different files and
        // agree only by convention; the first push the kernel makes goes to
        // `stack_top - 8`, and if that is past the last mapped page the
        // machine dies before its first line of output.
        let conf = crate::config::Config::parse(
            b"kernel_stack_address=0xffffff0100000000\nkernel_stack_size=512\n",
        );
        let top = conf.stack_top();
        let mut m = Fake::default();
        assert_eq!(
            map_stack(conf.kernel_stack_address, conf.kernel_stack_size, &mut m),
            Ok(())
        );
        assert!(
            m.frame_at((top - 8) & !(PAGE_SIZE - 1)).is_some(),
            "the kernel's first push lands on an unmapped page"
        );
        assert_eq!(
            m.frame_at(top & !(PAGE_SIZE - 1)),
            None,
            "the stack claims a page above its own top"
        );
        assert_eq!(m.pages().len(), conf.kernel_stack_size as usize);
    }

    #[test]
    fn an_unaligned_stack_base_still_covers_its_own_top() {
        let addr = KVA + 0x1800;
        let pages = 2;
        let mut m = Fake::default();
        assert_eq!(map_stack(addr, pages, &mut m), Ok(()));
        let top = addr + pages * PAGE_SIZE;
        assert!(m.frame_at((top - 8) & !(PAGE_SIZE - 1)).is_some());
    }

    #[test]
    fn stack_pages_are_present_and_writable() {
        let mut m = Fake::default();
        map_stack(KVA + 0x1000, 3, &mut m).unwrap();
        for page in m.pages() {
            assert_eq!(
                m.flags_at(page),
                Some(PageTableFlags::PRESENT | PageTableFlags::WRITABLE)
            );
        }
    }

    #[test]
    fn a_stack_of_zero_pages_is_refused() {
        let mut m = Fake::default();
        assert_eq!(
            map_stack(KVA + 0x1000, 0, &mut m),
            Err(MapError::EmptyStack)
        );
    }

    #[test]
    fn a_non_canonical_stack_address_is_refused_instead_of_panicking() {
        let bad = 0x0001_0000_0000_0000;
        let mut m = Fake::default();
        assert_eq!(map_stack(bad, 4, &mut m), Err(MapError::NotCanonical(bad)));
    }

    #[test]
    fn a_stack_that_runs_off_the_end_of_the_address_space_is_refused() {
        let addr = u64::MAX - 0xfff;
        let mut m = Fake::default();
        assert_eq!(
            map_stack(addr, 2, &mut m),
            Err(MapError::AddressOverflow(addr))
        );
    }

    #[test]
    fn a_stack_whose_size_alone_overflows_is_refused() {
        let mut m = Fake::default();
        assert_eq!(
            map_stack(KVA, u64::MAX / 2, &mut m),
            Err(MapError::AddressOverflow(KVA))
        );
    }

    #[test]
    fn running_out_of_frames_is_an_error_not_a_short_stack() {
        let mut m = Fake::with_frames(2);
        assert_eq!(
            map_stack(KVA + 0x1000, 4, &mut m),
            Err(MapError::OutOfFrames)
        );
    }

    // ----------------------------------------------------- physical memory

    #[test]
    fn physical_memory_is_mapped_frame_by_frame_at_the_offset() {
        let mut m = Fake::default();
        assert_eq!(map_physical_memory(KVA, 0x3000, &mut m), Ok(()));
        // `max_addr` is one past the last byte every caller cares about, and
        // its frame is included on purpose.
        assert_eq!(
            m.pages(),
            std::vec![KVA, KVA + 0x1000, KVA + 0x2000, KVA + 0x3000]
        );
        for i in 0..=3u64 {
            assert_eq!(m.frame_at(KVA + i * PAGE_SIZE), Some(i * PAGE_SIZE));
            assert_eq!(
                m.flags_at(KVA + i * PAGE_SIZE),
                Some(PageTableFlags::PRESENT | PageTableFlags::WRITABLE)
            );
        }
    }

    #[test]
    fn a_page_the_firmware_already_points_at_the_same_frame_is_accepted() {
        let mut m = Fake::default();
        m.preset(KVA + 0x1000, 0x1000);
        assert_eq!(map_physical_memory(KVA, 0x2000, &mut m), Ok(()));
        assert_eq!(m.frame_at(KVA + 0x1000), Some(0x1000));
    }

    #[test]
    fn a_page_the_firmware_points_somewhere_else_stops_the_mapping() {
        let mut m = Fake::default();
        m.preset(KVA + 0x1000, 0x9_0000);
        assert_eq!(
            map_physical_memory(KVA, 0x2000, &mut m),
            Err(MapError::AlreadyMapped {
                page: KVA + 0x1000,
                frame: 0x9_0000
            })
        );
    }

    #[test]
    fn a_non_canonical_physical_memory_offset_is_refused_instead_of_panicking() {
        let bad = 0x0001_0000_0000_0000;
        let mut m = Fake::default();
        assert_eq!(
            map_physical_memory(bad, 0x1000, &mut m),
            Err(MapError::NotCanonical(bad))
        );
    }

    #[test]
    fn a_physical_memory_offset_that_wraps_is_refused() {
        let offset = u64::MAX - 0xfff;
        let mut m = Fake::default();
        assert_eq!(
            map_physical_memory(offset, 0x2000, &mut m),
            Err(MapError::AddressOverflow(offset))
        );
    }

    #[test]
    fn a_max_addr_that_is_not_a_physical_address_is_refused() {
        let bad = 1u64 << 52;
        let mut m = Fake::default();
        assert_eq!(
            map_physical_memory(KVA, bad, &mut m),
            Err(MapError::NotPhysical(bad))
        );
    }

    #[test]
    fn errors_say_which_address_was_wrong() {
        use std::string::ToString;
        assert!(MapError::NotCanonical(0x1234)
            .to_string()
            .contains("0x1234"));
        assert!(MapError::MisalignedSegment {
            vaddr: 0xabc,
            offset: 0xdef
        }
        .to_string()
        .contains("0xdef"));
    }
}
