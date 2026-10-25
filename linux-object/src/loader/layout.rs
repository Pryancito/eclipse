//! Where an image's parts landed, and what its rows of `/proc/<pid>/maps`
//! are called.
//!
//! Linux keeps the first in `mm_struct` (`start_code`, `end_code`,
//! `start_data`, `end_data`, `start_brk`, `start_stack`): `/proc/<pid>/stat`
//! prints them as fields 26-28 and 45-47, and `maps` names the anonymous rows
//! that hold the break and the first stack pointer `[heap]` and `[stack]`.
//! None of it was kept here, so all six fields read 0 and neither row had a
//! name. Nor had the executable's own rows, or its dynamic linker's: the
//! loader built every segment's object itself and named none of them, where
//! Linux maps each from the file and `maps` shows its path and the offset in
//! it. `layout-survey` passed 1 of its 18 checks here and all 18 on Linux
//! 6.18, with glibc and with musl.

use alloc::{string::String, vec::Vec};
use xmas_elf::{program::Type, ElfFile};
use zircon_object::object::KernelObject;
use zircon_object::vm::{VmAddressRegion, PAGE_SIZE};

/// What `load_elf_binary` records of where an image went: Linux's
/// `mm->start_code` and the five that follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImageLayout {
    /// The lowest start of an executable segment.
    pub start_code: usize,
    /// The highest end of an executable segment's file bytes.
    pub end_code: usize,
    /// The highest start of any segment.
    pub start_data: usize,
    /// The highest end of any segment's file bytes.
    pub end_data: usize,
    /// The break the image started with.
    pub start_brk: usize,
    /// The first stack pointer: the address of `argc`.
    pub start_stack: usize,
}

/// One `PT_LOAD` header as the layout reads it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Segment {
    /// `PF_X`.
    pub exec: bool,
    /// `p_vaddr`.
    pub vaddr: u64,
    /// `p_filesz`: the ends are where the file's bytes end, not `p_memsz`.
    pub filesz: u64,
}

impl ImageLayout {
    /// The loop of `load_elf_binary` over the `PT_LOAD` headers, then the
    /// load bias added to all four. An image with no executable segment
    /// gets an empty code range at 0, where Linux leaves `~0` plus the bias:
    /// such an image faults at its entry before anything could read it.
    pub(super) fn of_segments(
        segments: impl Iterator<Item = Segment>,
        bias: usize,
        start_brk: usize,
        start_stack: usize,
    ) -> Self {
        let mut code: Option<(u64, u64)> = None;
        let (mut start_data, mut end_data) = (0u64, 0u64);
        for s in segments {
            let end = s.vaddr.saturating_add(s.filesz);
            if s.exec {
                let (lo, hi) = code.get_or_insert((s.vaddr, end));
                *lo = (*lo).min(s.vaddr);
                *hi = (*hi).max(end);
            }
            start_data = start_data.max(s.vaddr);
            end_data = end_data.max(end);
        }
        let at = |v: u64| bias.wrapping_add(v as usize);
        let (start_code, end_code) = code.map_or((0, 0), |(lo, hi)| (at(lo), at(hi)));
        ImageLayout {
            start_code,
            end_code,
            start_data: at(start_data),
            end_data: at(end_data),
            start_brk,
            start_stack,
        }
    }

    /// [`Self::of_segments`] for the `PT_LOAD` headers of `elf`.
    pub(super) fn of_elf(elf: &ElfFile, bias: usize, start_brk: usize, start_stack: usize) -> Self {
        Self::of_segments(segments(elf), bias, start_brk, start_stack)
    }
}

fn segments<'a>(elf: &'a ElfFile) -> impl Iterator<Item = Segment> + 'a {
    elf.program_iter()
        .filter(|ph| ph.get_type() == Ok(Type::Load))
        .map(|ph| Segment {
            exec: ph.flags().is_execute(),
            vaddr: ph.virtual_addr(),
            filesz: ph.file_size(),
        })
}

/// Give each segment of an image loaded at `bias` the name and file offset
/// `maps` shows for it: the file's path, and where in the file the row's
/// first byte is. That offset is what turns an address in the row into a
/// place in the file for a symbolizer, so a row with the name and not the
/// offset would send it to the wrong code. A segment with no file bytes is
/// anonymous memory in Linux, and stays unnamed.
pub(super) fn name_segments(vmar: &VmAddressRegion, elf: &ElfFile, bias: usize, name: &str) {
    for ph in elf.program_iter() {
        if ph.get_type() != Ok(Type::Load) || ph.file_size() == 0 {
            continue;
        }
        let Some(at) = bias.checked_add(ph.virtual_addr() as usize) else {
            continue;
        };
        if let Some(mapping) = vmar.find_mapping(at) {
            let page_offset = ph.virtual_addr() as usize % PAGE_SIZE;
            mapping.vmo().set_name(name);
            mapping
                .vmo()
                .set_file_offset((ph.offset() as usize).saturating_sub(page_offset));
        }
    }
}

/// Rename every row a freshly loaded image left named after a path to the
/// path `real` gives for it: `/proc/<pid>/exe`'s for the executable, the
/// file's own for its dynamic linker.
///
/// The loader names an image by the path it was handed, which is the one
/// the caller of `execve` wrote (relative, or through a link), and the
/// dynamic linker by its `PT_INTERP` (`/lib64/ld-linux-x86-64.so.2`, a link
/// on any glibc system). Linux shows the file's own path for both, and
/// `execve` only knows the executable's once the image is loaded: a
/// script's is its interpreter's. `real` is asked once per name; the
/// `[vdso]` and anonymous rows are left alone.
pub fn rename_images(vmar: &VmAddressRegion, real: impl Fn(&str) -> String) {
    let mut renamed: Vec<(String, String)> = Vec::new();
    for row in vmar.mappings_dump() {
        if row.name.is_empty() || row.name.starts_with('[') {
            continue;
        }
        let to = match renamed.iter().find(|(from, _)| *from == row.name) {
            Some((_, to)) => to.clone(),
            None => {
                let to = real(&row.name);
                renamed.push((row.name.clone(), to.clone()));
                to
            }
        };
        if to != row.name {
            if let Some(mapping) = vmar.find_mapping(row.start) {
                mapping.vmo().set_name(&to);
            }
        }
    }
}

#[cfg(test)]
mod tests;
