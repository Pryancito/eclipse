//! ELF Loader para cargar binarios en userspace

use crate::process::{current_process_id, get_process, ProcessId};
use crate::filesystem;
use crate::memory;
use crate::serial;
use alloc::vec::Vec;
use core::arch::asm;
use core::ptr::write_volatile;

/// ELF Header (64-bit)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Elf64Header {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

/// Program Header (64-bit)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Elf64ProgramHeader {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_DYNAMIC: u32 = 2;
const PT_TLS:  u32 = 7;   // Thread-Local Storage template segment
const PF_X: u32 = 1;  // Segment executable
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;

const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const DT_SYMTAB: i64 = 6;
const DT_STRTAB: i64 = 5;
const DT_SYMENT: i64 = 11;

const R_X86_64_64: u32 = 1;
const R_X86_64_GLOB_DAT: u32 = 6;
const R_X86_64_JUMP_SLOT: u32 = 7;
const R_X86_64_RELATIVE: u32 = 8;

const SHN_UNDEF: u16 = 0;

/// Resultado de cargar un ELF (estático o con PT_INTERP + intérprete dinámico).
#[derive(Clone, Copy, Debug)]
pub struct ExecLoadResult {
    pub entry_point: u64,
    pub max_vaddr: u64,
    pub phdr_va: u64,
    pub phnum: u64,
    pub phentsize: u64,
    pub segment_frames: u64,
    pub tls_base: u64,
    /// `Some((AT_BASE, AT_ENTRY))` cuando el arranque es el intérprete (p. ej. ld-musl).
    pub dynamic_linker: Option<(u64, u64)>,
    /// Rangos de memoria virtual ocupados por los segmentos ELF cargados (para registrar como VMAs).
    /// Para binarios estáticos: 1 rango [inicio_segmento, max_vaddr).
    /// Para binarios dinámicos: hasta 2 rangos (binario principal + intérprete).
    pub loaded_vma_ranges: [(u64, u64); 2],
    pub loaded_vma_count: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Dyn {
    d_tag: i64,
    d_val: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Sym {
    st_name: u32,
    st_info: u8,
    st_other: u8,
    st_shndx: u16,
    st_value: u64,
    st_size: u64,
}
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const USER_ADDR_MAX: u64 = 0x0000_7FFF_FFFF_FFFF;
/// Minimum sane entry point; anything below is inside ELF header (64 bytes) or bogus
const MIN_ENTRY_POINT: u64 = 0x80;

/// Carga típica del programa principal PIE con intérprete (evita solaparse con ld.so).
const DYNAMIC_MAIN_LOAD_BIAS: u64 = 0x4000_0000;
/// Separación mínima entre fin de imagen principal e intérprete.
const DYNAMIC_INTERP_GAP: u64 = 0x1000_0000;

fn r_sym(info: u64) -> usize {
    (info >> 32) as usize
}

fn r_type(info: u64) -> u32 {
    (info & 0xffff_ffff) as u32
}

fn va_to_file_off(
    elf_data: &[u8],
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
    vaddr: u64,
) -> Option<usize> {
    for i in 0..ph_count {
        let off = ph_offset + i * ph_size;
        if off + ph_size > elf_data.len() {
            return None;
        }
        let ph = unsafe { &*(elf_data[off..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type == PT_LOAD && vaddr >= ph.p_vaddr && vaddr < ph.p_vaddr.saturating_add(ph.p_filesz) {
            return Some((ph.p_offset + (vaddr - ph.p_vaddr)) as usize);
        }
    }
    None
}

fn write_user_u64(page_table_phys: u64, vaddr: u64, value: u64) -> Result<(), &'static str> {
    if (vaddr & 7) != 0 {
        return Err("ELF: unaligned reloc target");
    }
    let page = vaddr & !0xFFF;
    let page_off = (vaddr & 0xFFF) as usize;
    let Some(phys) = crate::memory::get_user_page_phys(page_table_phys, page) else {
        return Err("ELF: reloc target not mapped");
    };
    let kptr = crate::memory::phys_to_virt(phys) as *mut u64;
    unsafe {
        kptr.add(page_off / 8).write_volatile(value);
    }
    Ok(())
}

fn min_load_vaddr(
    elf_data: &[u8],
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
    load_bias: u64,
) -> u64 {
    for i in 0..ph_count {
        let off = ph_offset + i * ph_size;
        if off + ph_size > elf_data.len() {
            break;
        }
        let ph = unsafe { &*(elf_data[off..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type == PT_LOAD {
            return ph.p_vaddr.wrapping_add(load_bias);
        }
    }
    0
}

fn extract_interp_path<'a>(
    elf_data: &'a [u8],
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
) -> Result<Option<&'a str>, &'static str> {
    for i in 0..ph_count {
        let off = ph_offset + i * ph_size;
        let ph = unsafe { &*(elf_data[off..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type != PT_INTERP {
            continue;
        }
        let start = ph.p_offset as usize;
        let end = start.saturating_add(ph.p_filesz as usize);
        if end > elf_data.len() || start >= elf_data.len() {
            return Err("ELF: PT_INTERP out of bounds");
        }
        let bytes = &elf_data[start..end];
        let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let path = core::str::from_utf8(&bytes[..nul]).map_err(|_| "ELF: PT_INTERP not UTF-8")?;
        return Ok(Some(path));
    }
    Ok(None)
}

fn has_pt_interp(elf_data: &[u8], ph_offset: usize, ph_count: usize, ph_size: usize) -> bool {
    extract_interp_path(elf_data, ph_offset, ph_count, ph_size)
        .ok()
        .flatten()
        .is_some()
}

fn validate_entry_for_load(
    elf_data: &[u8],
    header: &Elf64Header,
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
    load_bias: u64,
    et_dyn: bool,
) -> Result<(), &'static str> {
    if header.e_entry < MIN_ENTRY_POINT {
        return Err("ELF: Entry point in header or invalid (e_entry < 0x80)");
    }
    let runtime_entry = if et_dyn {
        load_bias.saturating_add(header.e_entry)
    } else {
        header.e_entry
    };
    if runtime_entry > USER_ADDR_MAX {
        return Err("ELF: entry in kernel space");
    }
    let mut ok = false;
    for i in 0..ph_count {
        let off = ph_offset + i * ph_size;
        let ph = unsafe { &*(elf_data[off..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type == PT_LOAD && (ph.p_flags & PF_X) != 0 {
            let base = if et_dyn {
                ph.p_vaddr.wrapping_add(load_bias)
            } else {
                ph.p_vaddr
            };
            let end = base.saturating_add(ph.p_memsz);
            if runtime_entry >= base && runtime_entry < end {
                ok = true;
                break;
            }
        }
    }
    if !ok {
        return Err("ELF: Entry point not in executable segment");
    }
    Ok(())
}

/// Map PT_LOAD; `et_dyn` adds `load_bias` to each segment vaddr. `load_bias` must be 0 for ET_EXEC.
fn map_pt_load_segments(
    page_table_phys: u64,
    elf_data: &[u8],
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
    load_bias: u64,
    et_dyn: bool,
) -> Result<(u64, u32), &'static str> {
    let mut mapped_count: u32 = 0;
    let mut max_vaddr: u64 = 0;
    for i in 0..ph_count {
        let ph_offset_entry = ph_offset + i * ph_size;
        let ph = unsafe { &*(elf_data[ph_offset_entry..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type != PT_LOAD {
            continue;
        }
        let vaddr_start = if et_dyn {
            ph.p_vaddr.wrapping_add(load_bias)
        } else {
            ph.p_vaddr
        };
        let vaddr_end = vaddr_start.saturating_add(ph.p_memsz);
        if vaddr_end > USER_ADDR_MAX {
            return Err("ELF: segment in kernel space");
        }
        serial::serial_printf(format_args!(
            "[load_elf] segment {}: vaddr=0x{:x} filesz=0x{:x} memsz=0x{:x}\n",
            i, vaddr_start, ph.p_filesz, ph.p_memsz
        ));
        let file_size = ph.p_filesz;
        let file_start_offset = ph.p_offset;
        if vaddr_end > max_vaddr {
            max_vaddr = vaddr_end;
        }
        let page_start = vaddr_start & !0xFFF;
        let page_end = (vaddr_end + 0xFFF) & !0xFFF;
        let mut current_vaddr = page_start;
        while current_vaddr < page_end {
            let (phys, allocated_new) =
                if let Some(existing_phys) = crate::memory::get_user_page_phys(page_table_phys, current_vaddr) {
                    (existing_phys, false)
                } else if let Some(new_phys) = crate::memory::alloc_phys_frame_for_anon_mmap() {
                    (new_phys, true)
                } else {
                    serial::serial_printf(format_args!(
                        "[load_elf] ERROR: allocation failed at 0x{:x}\n",
                        current_vaddr
                    ));
                    return Err("Failed to allocate 4KB anonymous frame for segment");
                };
            let kptr = crate::memory::phys_to_virt(phys) as *mut u8;
            if allocated_new {
                unsafe { core::ptr::write_bytes(kptr, 0, 0x1000) };
                mapped_count = mapped_count.saturating_add(1);
            }
            // Always (re)map: if the VPN already had a PTE (e.g. leftover from a prior mapping),
            // we must refresh WRITABLE|USER or segment BSS writes fault with #PF err=protection.
            crate::memory::map_user_page_4kb(
                page_table_phys,
                current_vaddr,
                phys,
                crate::memory::PAGE_WRITABLE | crate::memory::PAGE_USER,
            );
            if file_size > 0 {
                let page_vaddr_start = current_vaddr;
                let page_vaddr_end = current_vaddr + 0x1000;
                let intersect_start = core::cmp::max(vaddr_start, page_vaddr_start);
                let intersect_end = core::cmp::min(vaddr_start + file_size, page_vaddr_end);
                if intersect_start < intersect_end {
                    let copy_size = (intersect_end - intersect_start) as usize;
                    let in_file_offset = (intersect_start - vaddr_start) as usize;
                    let in_page_offset = (intersect_start - page_vaddr_start) as usize;
                    let file_src_start = file_start_offset as usize + in_file_offset;
                    let file_src_end = file_src_start.saturating_add(copy_size);
                    if file_src_end > elf_data.len() {
                        return Err("ELF: segment data extends past end of file");
                    }
                    unsafe {
                        let src = elf_data.as_ptr().add(file_src_start);
                        let dst = kptr.add(in_page_offset);
                        core::ptr::copy_nonoverlapping(src, dst, copy_size);
                    }
                }
            }
            current_vaddr += 0x1000;
        }
    }
    Ok((max_vaddr, mapped_count))
}

fn tls_setup_after_load(
    page_table_phys: u64,
    elf_data: &[u8],
    max_vaddr_aligned: u64,
    tls_filesz: u64,
    tls_memsz: u64,
    tls_file_offset: u64,
    tls_align: u64,
    has_tls: bool,
    mapped_count: &mut u32,
) -> u64 {
    if !has_tls || tls_memsz == 0 {
        return 0;
    }
    let tls_align = if tls_align >= 2 && tls_align.is_power_of_two() {
        tls_align
    } else {
        8
    };
    let aligned_memsz = (tls_memsz + tls_align - 1) & !(tls_align - 1);
    const TCB_SIZE: u64 = 8;
    let tls_total = aligned_memsz + TCB_SIZE;
    let tls_virt_start = max_vaddr_aligned + 0x1000;
    let tls_pages = ((tls_total + 0xFFF) & !0xFFF) / 0x1000;
    let mut tls_ok = true;
    for i in 0..tls_pages as u64 {
        match crate::memory::alloc_phys_frame_for_anon_mmap() {
            Some(phys) => {
                let vaddr = tls_virt_start + i * 0x1000;
                unsafe {
                    core::ptr::write_bytes(crate::memory::phys_to_virt(phys) as *mut u8, 0, 0x1000);
                }
                crate::memory::map_user_page_4kb(
                    page_table_phys,
                    vaddr,
                    phys,
                    crate::memory::PAGE_WRITABLE | crate::memory::PAGE_USER,
                );
                *mapped_count = mapped_count.saturating_add(1);
            }
            None => {
                tls_ok = false;
                break;
            }
        }
    }
    if !tls_ok {
        serial::serial_print("[load_elf] WARNING: TLS page alloc failed — TLS disabled\n");
        return 0;
    }
    if tls_filesz > 0 {
        let src_end = (tls_file_offset as usize).saturating_add(tls_filesz as usize);
        if src_end <= elf_data.len() {
            let mut src_off = 0usize;
            while src_off < tls_filesz as usize {
                let dst_vaddr = tls_virt_start + src_off as u64;
                let page_vaddr = dst_vaddr & !0xFFF;
                let page_off = (dst_vaddr & 0xFFF) as usize;
                if let Some(phys) = crate::memory::get_user_page_phys(page_table_phys, page_vaddr) {
                    let kptr = crate::memory::phys_to_virt(phys) as *mut u8;
                    let space = 0x1000 - page_off;
                    let remain = tls_filesz as usize - src_off;
                    let chunk = space.min(remain);
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            elf_data.as_ptr().add(tls_file_offset as usize + src_off),
                            kptr.add(page_off),
                            chunk,
                        );
                    }
                    src_off += chunk;
                } else {
                    break;
                }
            }
        }
    }
    let tcb_virt = tls_virt_start + aligned_memsz;
    let tcb_pg_vaddr = tcb_virt & !0xFFF;
    let tcb_pg_off = (tcb_virt & 0xFFF) as usize;
    if let Some(phys) = crate::memory::get_user_page_phys(page_table_phys, tcb_pg_vaddr) {
        let kptr = crate::memory::phys_to_virt(phys) as *mut u8;
        unsafe {
            core::ptr::write_unaligned(kptr.add(tcb_pg_off) as *mut u64, tcb_virt);
        }
    }
    serial::serial_printf(format_args!(
        "[load_elf] TLS: memsz={} filesz={} align={} tls_base=0x{:x}\n",
        tls_memsz, tls_filesz, tls_align, tcb_virt,
    ));
    tcb_virt
}

fn collect_tls_phdr(
    elf_data: &[u8],
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
) -> (bool, u64, u64, u64, u64) {
    let mut tls_filesz = 0u64;
    let mut tls_memsz = 0u64;
    let mut tls_file_offset = 0u64;
    let mut tls_align = 8u64;
    let mut has_tls = false;
    for i in 0..ph_count {
        let off = ph_offset + i * ph_size;
        let ph = unsafe { &*(elf_data[off..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type == PT_TLS {
            tls_filesz = ph.p_filesz;
            tls_memsz = ph.p_memsz;
            tls_file_offset = ph.p_offset;
            tls_align = if ph.p_align > 1 { ph.p_align } else { 8 };
            has_tls = true;
            break;
        }
    }
    (has_tls, tls_filesz, tls_memsz, tls_file_offset, tls_align)
}

fn apply_relocations_for_load_base(
    page_table_phys: u64,
    elf_data: &[u8],
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
    load_base: u64,
) -> Result<(), &'static str> {
    let mut dyn_file_off: Option<(u64, u64)> = None;
    for i in 0..ph_count {
        let off = ph_offset + i * ph_size;
        let ph = unsafe { &*(elf_data[off..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type == PT_DYNAMIC {
            dyn_file_off = Some((ph.p_offset, ph.p_filesz));
            break;
        }
    }
    let Some((d_off, d_filesz)) = dyn_file_off else {
        return Ok(());
    };
    if d_filesz < core::mem::size_of::<Elf64Dyn>() as u64 {
        return Ok(());
    }
    let d_start = d_off as usize;
    let d_end = d_start.saturating_add(d_filesz as usize);
    if d_end > elf_data.len() {
        return Err("ELF: PT_DYNAMIC out of bounds");
    }
    let mut dt_rela: Option<u64> = None;
    let mut dt_relasz: u64 = 0;
    let mut dt_relaent: u64 = 0;
    let mut dt_symtab: Option<u64> = None;
    let mut dt_strtab: Option<u64> = None;
    let mut dt_syment: u64 = core::mem::size_of::<Elf64Sym>() as u64;
    let mut idx = d_start;
    while idx + core::mem::size_of::<Elf64Dyn>() <= d_end {
        let d = unsafe { &*(elf_data[idx..].as_ptr() as *const Elf64Dyn) };
        match d.d_tag {
            DT_NULL => break,
            DT_RELA => dt_rela = Some(d.d_val),
            DT_RELASZ => dt_relasz = d.d_val,
            DT_RELAENT => dt_relaent = d.d_val,
            DT_SYMTAB => dt_symtab = Some(d.d_val),
            DT_STRTAB => dt_strtab = Some(d.d_val),
            DT_SYMENT => {
                if d.d_val != 0 {
                    dt_syment = d.d_val;
                }
            }
            _ => {}
        }
        idx += core::mem::size_of::<Elf64Dyn>();
    }
    let Some(rela_vaddr) = dt_rela else {
        return Ok(());
    };
    if dt_relasz == 0 {
        return Ok(());
    }
    if dt_relaent == 0 {
        dt_relaent = core::mem::size_of::<Elf64Rela>() as u64;
    }
    let rela_file_off = va_to_file_off(elf_data, ph_offset, ph_count, ph_size, rela_vaddr)
        .ok_or("ELF: DT_RELA not in file")?;
    let symtab_vaddr = dt_symtab.ok_or("ELF: missing DT_SYMTAB for reloc")?;
    let symtab_file_off = va_to_file_off(elf_data, ph_offset, ph_count, ph_size, symtab_vaddr)
        .ok_or("ELF: DT_SYMTAB not in file")?;
    dt_strtab.ok_or("ELF: missing DT_STRTAB for reloc")?;

    let mut rel_off = rela_file_off;
    let rel_end = rela_file_off.saturating_add(dt_relasz as usize);
    while rel_off + core::mem::size_of::<Elf64Rela>() <= rel_end.min(elf_data.len()) {
        let rela = unsafe { &*(elf_data[rel_off..].as_ptr() as *const Elf64Rela) };
        let typ = r_type(rela.r_info);
        let sym_idx = r_sym(rela.r_info);
        let target = load_base.wrapping_add(rela.r_offset);
        match typ {
            R_X86_64_RELATIVE => {
                let v = load_base.wrapping_add(rela.r_addend as u64);
                write_user_u64(page_table_phys, target, v)?;
            }
            R_X86_64_GLOB_DAT | R_X86_64_JUMP_SLOT | R_X86_64_64 => {
                let sym_off = symtab_file_off.saturating_add(sym_idx.saturating_mul(dt_syment as usize));
                if sym_off + core::mem::size_of::<Elf64Sym>() > elf_data.len() {
                    return Err("ELF: symbol out of bounds");
                }
                let sym = unsafe { &*(elf_data[sym_off..].as_ptr() as *const Elf64Sym) };
                let s = if sym.st_shndx == SHN_UNDEF {
                    return Err("ELF: unresolved reloc (undef sym)");
                } else {
                    load_base.wrapping_add(sym.st_value)
                };
                let v = s.wrapping_add(rela.r_addend as u64);
                write_user_u64(page_table_phys, target, v)?;
            }
            0 => {}
            _ => {
                serial::serial_printf(format_args!(
                    "[load_elf] WARN: unhandled reloc type {} at off 0x{:x}\n",
                    typ, rela.r_offset
                ));
            }
        }
        rel_off = rel_off.saturating_add(dt_relaent as usize);
    }
    Ok(())
}

fn phdr_va_biased(elf_data: &[u8], load_bias: u64, et_dyn: bool) -> Result<(u64, u64, u64), &'static str> {
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        return Err("ELF: file too small");
    }
    let header = unsafe { &*(elf_data.as_ptr() as *const Elf64Header) };
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    if elf_data.len() < ph_offset + ph_count * ph_size {
        return Err("ELF: phdr out of bounds");
    }
    let mut first_load_vaddr = None;
    for i in 0..ph_count {
        let off = ph_offset + i * ph_size;
        let ph = unsafe { &*(elf_data[off..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type == PT_LOAD {
            first_load_vaddr = Some(ph.p_vaddr);
            break;
        }
    }
    let first = first_load_vaddr.ok_or("ELF: no PT_LOAD")?;
    let phdr_va = if et_dyn {
        first.wrapping_add(load_bias).wrapping_add(header.e_phoff)
    } else {
        first.wrapping_add(header.e_phoff)
    };
    Ok((phdr_va, header.e_phnum as u64, header.e_phentsize as u64))
}

fn load_elf_dynamic_pair(page_table_phys: u64, main_elf: &[u8], interp_path: &str) -> Result<ExecLoadResult, &'static str> {
    serial::serial_printf(format_args!(
        "[load_elf] dynamic: loading interpreter \"{}\"\n",
        interp_path
    ));
    let interp_elf: Vec<u8> = filesystem::read_file_alloc(interp_path)?;
    if interp_elf.len() < core::mem::size_of::<Elf64Header>() {
        return Err("ELF: interpreter too small");
    }
    let main_h = unsafe { &*(main_elf.as_ptr() as *const Elf64Header) };
    let interp_h = unsafe { &*(interp_elf.as_ptr() as *const Elf64Header) };
    if &interp_h.e_ident[0..4] != &ELF_MAGIC {
        return Err("ELF: interpreter bad magic");
    }
    if interp_h.e_ident[4] != 2 {
        return Err("ELF: interpreter not 64-bit");
    }
    let main_ph_off = main_h.e_phoff as usize;
    let main_ph_count = main_h.e_phnum as usize;
    let main_ph_size = main_h.e_phentsize as usize;
    let interp_ph_off = interp_h.e_phoff as usize;
    let interp_ph_count = interp_h.e_phnum as usize;
    let interp_ph_size = interp_h.e_phentsize as usize;

    if has_pt_interp(&interp_elf, interp_ph_off, interp_ph_count, interp_ph_size) {
        return Err("ELF: nested PT_INTERP in interpreter");
    }

    let main_et_dyn = main_h.e_type == ET_DYN;
    let main_bias = if main_et_dyn {
        DYNAMIC_MAIN_LOAD_BIAS
    } else {
        0u64
    };
    if !main_et_dyn && main_h.e_type != ET_EXEC {
        return Err("ELF: main not ET_EXEC/ET_DYN");
    }
    validate_entry_for_load(
        main_elf,
        main_h,
        main_ph_off,
        main_ph_count,
        main_ph_size,
        main_bias,
        main_et_dyn,
    )?;

    let interp_et_dyn = interp_h.e_type == ET_DYN;
    if !interp_et_dyn && interp_h.e_type != ET_EXEC {
        return Err("ELF: interpreter not ET_EXEC/ET_DYN");
    }
    validate_entry_for_load(
        &interp_elf,
        interp_h,
        interp_ph_off,
        interp_ph_count,
        interp_ph_size,
        0,
        interp_et_dyn,
    )?;

    let (max_v_main, mut mapped) = map_pt_load_segments(
        page_table_phys,
        main_elf,
        main_ph_off,
        main_ph_count,
        main_ph_size,
        main_bias,
        main_et_dyn,
    )?;
    let max_v_main_al = (max_v_main + 0xFFF) & !0xFFF;
    // No duplicar PT_TLS en un bloque aparte ni fijar %fs aquí: el primer código que corre es ld-musl,
    // que debe ver %fs=0 (o su propio TLS) hasta montar el hilo inicial. Si cargamos el TLS del main
    // y ponemos FS_BASE en el TCB del main, ld.so usa offsets propios de libc y acaba leyendo/escribiendo
    // fuera del bloque (p. ej. CR2≈0x409d1ff8 con TCB en 0x409d3180).
    let tls_base: u64 = 0;
    let main_end = max_v_main_al.saturating_add(0x1000);

    let base_above_main = main_end.saturating_add(DYNAMIC_INTERP_GAP);
    let step = 0x1_0000_0000u64;
    let interp_bias = (base_above_main.saturating_add(step - 1) / step).saturating_mul(step);
    serial::serial_printf(format_args!(
        "[load_elf] dynamic: main_bias=0x{:x} interp_bias=0x{:x} main_end=0x{:x}\n",
        main_bias, interp_bias, main_end
    ));

    let (max_v_interp, mapped_i) = map_pt_load_segments(
        page_table_phys,
        &interp_elf,
        interp_ph_off,
        interp_ph_count,
        interp_ph_size,
        interp_bias,
        interp_et_dyn,
    )?;
    mapped = mapped.saturating_add(mapped_i);
    // apply_relocations_for_load_base(
    //     page_table_phys,
    //     &interp_elf,
    //     interp_ph_off,
    //     interp_ph_count,
    //     interp_ph_size,
    //     interp_bias,
    // )?;

    let interp_entry = if interp_et_dyn {
        interp_bias.wrapping_add(interp_h.e_entry)
    } else {
        interp_h.e_entry
    };
    let main_entry_va = if main_et_dyn {
        main_bias.wrapping_add(main_h.e_entry)
    } else {
        main_h.e_entry
    };
    let max_v_interp_al = (max_v_interp + 0xFFF) & !0xFFF;
    let max_maps = core::cmp::max(max_v_main_al, max_v_interp_al);

    let (phdr_va, phnum, phentsize) = phdr_va_biased(main_elf, main_bias, main_et_dyn)?;

    serial::serial_printf(format_args!(
        "[load_elf] dynamic: interp_entry=0x{:x} main_entry=0x{:x} phdr=0x{:x}\n",
        interp_entry, main_entry_va, phdr_va
    ));

    Ok(ExecLoadResult {
        entry_point: interp_entry,
        max_vaddr: max_maps,
        phdr_va,
        phnum,
        phentsize,
        segment_frames: mapped as u64,
        tls_base,
        dynamic_linker: Some((interp_bias, main_entry_va)),
        loaded_vma_ranges: [(main_bias, max_v_main_al), (interp_bias, max_v_interp_al)],
        loaded_vma_count: 2,
    })
}

/// Cargar los segmentos del ELF en el espacio de direcciones especificado.
pub fn load_elf_into_space(page_table_phys: u64, elf_data: &[u8]) -> Result<ExecLoadResult, &'static str> {
    serial::serial_printf(format_args!(
        "[load_elf] start len={} cr3=0x{:x}\n",
        elf_data.len(),
        page_table_phys
    ));
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        return Err("ELF: File too small");
    }
    let header = unsafe { &*(elf_data.as_ptr() as *const Elf64Header) };
    if &header.e_ident[0..4] != &ELF_MAGIC {
        return Err("ELF: Invalid magic number");
    }
    if header.e_ident[4] != 2 {
        return Err("ELF: not 64-bit");
    }
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    if elf_data.len() < ph_offset + ph_count * ph_size {
        return Err("ELF: Program headers out of bounds");
    }

    if let Some(interp_path) = extract_interp_path(elf_data, ph_offset, ph_count, ph_size)? {
        return load_elf_dynamic_pair(page_table_phys, elf_data, interp_path);
    }

    let et_dyn = header.e_type == ET_DYN;
    let load_bias = 0u64;
    if header.e_type != ET_EXEC && !et_dyn {
        return Err("ELF: not ET_EXEC/ET_DYN");
    }
    validate_entry_for_load(
        elf_data,
        header,
        ph_offset,
        ph_count,
        ph_size,
        load_bias,
        et_dyn,
    )?;

    let (has_tls, tf, tm, tfo, ta) = collect_tls_phdr(elf_data, ph_offset, ph_count, ph_size);
    let (max_v, mut mapped) = map_pt_load_segments(
        page_table_phys,
        elf_data,
        ph_offset,
        ph_count,
        ph_size,
        load_bias,
        et_dyn,
    )?;
    let max_vaddr_aligned = (max_v + 0xFFF) & !0xFFF;
    let entry_point = if et_dyn {
        load_bias.wrapping_add(header.e_entry)
    } else {
        header.e_entry
    };

    let tls_base = tls_setup_after_load(
        page_table_phys,
        elf_data,
        max_vaddr_aligned,
        tf,
        tm,
        tfo,
        ta,
        has_tls,
        &mut mapped,
    );

    let (phdr_va, phnum, phentsize) = phdr_va_biased(elf_data, load_bias, et_dyn)?;

    serial::serial_printf(format_args!(
        "[load_elf] successfully loaded: entry=0x{:x} max_v=0x{:x} mapped_pages={}\n",
        entry_point, max_vaddr_aligned, mapped
    ));

    let min_vaddr = min_load_vaddr(elf_data, ph_offset, ph_count, ph_size, load_bias);

    Ok(ExecLoadResult {
        entry_point,
        max_vaddr: max_vaddr_aligned,
        phdr_va,
        phnum,
        phentsize,
        segment_frames: mapped as u64,
        tls_base,
        dynamic_linker: None,
        loaded_vma_ranges: [(min_vaddr, max_vaddr_aligned), (0, 0)],
        loaded_vma_count: 1,
    })
}

/// Preparar el stack de usuario
pub fn setup_user_stack(page_table_phys: u64, stack_base: u64, stack_size: usize) -> Result<u64, &'static str> {
    serial::serial_printf(format_args!("[setup_stack] start base=0x{:x} size={} pages={}\n", stack_base, stack_size, stack_size / 4096));
    for i in 0..(stack_size / 4096) {
        if let Some(phys) = crate::memory::alloc_phys_frame_for_anon_mmap() {
            let offset = (i as u64) * 0x1000;
            crate::memory::map_user_page_4kb(
                page_table_phys, 
                stack_base + offset, 
                phys, 
                crate::memory::PAGE_WRITABLE | crate::memory::PAGE_USER
            );
        } else {
            serial::serial_printf(format_args!("[setup_stack] alloc failed at page {}\n", i));
            return Err("Failed to allocate 4KB anonymous frame for user stack");
        }
    }
    serial::serial_print("[setup_stack] done\n");
    Ok(stack_base + stack_size as u64)
}

/// Cargar binario ELF en memoria y crear proceso
pub fn load_elf(elf_data: &[u8]) -> Option<ProcessId> {
    // Create the page table for this process ONCE and reuse it throughout.
    // CRITICAL: We must pass this same cr3 to create_process_with_pid.
    // Previously load_elf called create_process which allocated a *second* fresh
    // cr3, causing the process to run in an empty address space while the ELF
    // segments lived in the discarded first cr3 → execution of zero bytes → crash.
    let cr3 = crate::memory::create_process_paging();

    let loaded = match load_elf_into_space(cr3, elf_data) {
        Ok(res) => res,
        Err(e) => {
            serial::serial_print("ELF: Load failed: ");
            serial::serial_print(e);
            serial::serial_print("\n");
            return None;
        }
    };

    // Default user stack at 512MB
    let stack_base = 0x20000000; // 512MB
    let stack_size = 0x100000;  // 1MB (previously 256KB)

    // Map the user stack into the SAME cr3 before creating the process entry.
    if let Err(e) = setup_user_stack(cr3, stack_base, stack_size) {
        serial::serial_print("ELF: Stack setup failed: ");
        serial::serial_print(e);
        serial::serial_print("\n");
        return None;
    }

    // Allocate a pid and register the process, reusing the cr3 we already set up.
    let pid = crate::process::next_pid();
    if !crate::process::create_process_with_pid(
        pid,
        cr3,
        loaded.entry_point,
        stack_base,
        stack_size,
        loaded.phdr_va,
        loaded.phnum,
        loaded.phentsize,
        loaded.max_vaddr,
        loaded.tls_base,
        loaded.dynamic_linker,
    ) {
        serial::serial_print("ELF: create_process_with_pid failed\n");
        return None;
    }

    // Add segment frames and TLS base to the process accounting.
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = crate::process::PROCESS_TABLE.lock();
        if let Some(p) = table[pid as usize].as_mut() {
            p.mem_frames += loaded.segment_frames;
            if loaded.tls_base != 0 {
                p.fs_base = loaded.tls_base;
            }
            p.dynamic_linker_aux = loaded.dynamic_linker;
        }
    });

    crate::fd::fd_init_stdio(pid);

    Some(pid)
}

fn load_info(elf_data: &[u8]) -> Result<(u64, usize, usize, usize), &'static str> {
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        return Err("ELF: File too small");
    }
    
    let header = unsafe {
        &*(elf_data.as_ptr() as *const Elf64Header)
    };
    
    if &header.e_ident[0..4] != &ELF_MAGIC {
        return Err("ELF: Invalid magic number");
    }
    
    Ok((header.e_entry, header.e_phoff as usize, header.e_phnum as usize, header.e_phentsize as usize))
}

/// Inicializar ELF loader
pub fn init() {
    serial::serial_print("ELF loader initialized\n");
}

/// Return (phdr_va, phnum, phentsize) for an ELF so the kernel can put AT_PHDR/AT_PHNUM/AT_PHENT in auxv.
/// phdr_va = first PT_LOAD.p_vaddr + e_phoff (program headers live at that VA in the loaded image).
pub fn get_elf_phdr_info(elf_data: &[u8]) -> Result<(u64, u64, u64), &'static str> {
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        return Err("ELF: file too small");
    }
    let header = unsafe { &*(elf_data.as_ptr() as *const Elf64Header) };
    if &header.e_ident[0..4] != &ELF_MAGIC {
        return Err("ELF: invalid magic");
    }
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    if elf_data.len() < ph_offset + ph_count * ph_size {
        return Err("ELF: phdr out of bounds");
    }
    let mut first_load_vaddr = None;
    for i in 0..ph_count {
        let off = ph_offset + i * ph_size;
        let ph = unsafe { &*(elf_data[off..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type == PT_LOAD {
            first_load_vaddr = Some(ph.p_vaddr);
            break;
        }
    }
    let first_load_vaddr = first_load_vaddr.ok_or("ELF: no PT_LOAD")?;
    let phdr_va = first_load_vaddr + header.e_phoff;
    Ok((phdr_va, header.e_phnum as u64, header.e_phentsize as u64))
}

/// Reemplazar la imagen del proceso actual con un nuevo ELF (backend de `exec`).
pub fn replace_process_image(pid: ProcessId, elf_data: &[u8]) -> Result<ExecLoadResult, &'static str> {
    serial::serial_printf(format_args!("[exec] replace_process_image for PID {}\n", pid));
    load_elf_into_space(crate::memory::get_cr3(), elf_data)
}

/// Salto al intérprete dinámico (p. ej. ld-musl): mismo ABI de args que [`jump_to_userspace`],
/// más `AT_BASE` / `AT_ENTRY` en auxv leídos de `Process::dynamic_linker_aux`.
///
/// # Safety
/// Igual que `jump_to_userspace`. Requiere `dynamic_linker_aux == Some((AT_BASE, AT_ENTRY))`.
pub unsafe extern "C" fn jump_to_userspace_dynamic_linker(
    entry_point: u64,
    stack_top: u64,
    phdr_va: u64,
    phnum: u64,
    phentsize: u64,
) -> ! {
    let _pid = current_process_id().unwrap_or(0xFFFF);
    crate::serial::serial_printf(format_args!(
        "[ELF] PID {} dynamic linker jump entry={:#x} stack_top={:#x} phdr={:#x}\n",
        _pid, entry_point, stack_top, phdr_va
    ));
    if entry_point == 0 {
        crate::serial::serial_print("ERROR: dynamic linker entry is ZERO\n");
        loop {
            core::arch::asm!("hlt");
        }
    }
    if entry_point >= USER_ADDR_MAX {
        serial::serial_print("ERROR: dynamic linker entry in kernel space\n");
        loop {
            core::arch::asm!("hlt");
        }
    }

    const AT_PAGESZ: u64 = 6;
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_BASE: u64 = 7;
    const AT_FLAGS: u64 = 8;
    const AT_ENTRY: u64 = 9;
    const AT_RANDOM: u64 = 25;
    const AT_NULL: u64 = 0;

    // Stack layout: argc (1), argv[0] (ptr), NULL (argv term), NULL (envp term), auxv...
    let adjusted_stack = (stack_top - 256) & !0xF;
    let program_ptr = adjusted_stack + 240;
    let random_ptr = adjusted_stack + 224;

    let (tls_base, at_base, at_entry) = if let Some(pid) = current_process_id() {
        if let Some(proc) = get_process(pid) {
            unsafe {
                memory::set_cr3(proc.resources.lock().page_table_phys);
            }
            match proc.dynamic_linker_aux {
                Some((b, e)) => (proc.fs_base, b, e),
                None => {
                    crate::serial::serial_print("ERROR: jump_to_userspace_dynamic_linker without dynamic_linker_aux\n");
                    loop {
                        core::arch::asm!("hlt");
                    }
                }
            }
        } else {
            (0u64, 0u64, 0u64)
        }
    } else {
        (0u64, 0u64, 0u64)
    };

    unsafe {
        let stack_ptr = adjusted_stack as *mut u64;
        write_volatile(stack_ptr.offset(0), 1u64);         // argc = 1
        write_volatile(stack_ptr.offset(1), program_ptr); // argv[0] = "program"
        write_volatile(stack_ptr.offset(2), 0u64);         // argv[1] = NULL
        write_volatile(stack_ptr.offset(3), 0u64);         // envp[0] = NULL
        
        write_volatile(stack_ptr.offset(4), AT_PHDR);
        write_volatile(stack_ptr.offset(5), phdr_va);
        write_volatile(stack_ptr.offset(6), AT_PHENT);
        write_volatile(stack_ptr.offset(7), phentsize);
        write_volatile(stack_ptr.offset(8), AT_PHNUM);
        write_volatile(stack_ptr.offset(9), phnum);
        write_volatile(stack_ptr.offset(10), AT_PAGESZ);
        write_volatile(stack_ptr.offset(11), 4096u64);
        write_volatile(stack_ptr.offset(12), AT_BASE);
        write_volatile(stack_ptr.offset(13), at_base);
        write_volatile(stack_ptr.offset(14), AT_FLAGS);
        write_volatile(stack_ptr.offset(15), 0u64);
        write_volatile(stack_ptr.offset(16), AT_ENTRY);
        write_volatile(stack_ptr.offset(17), at_entry);
        write_volatile(stack_ptr.offset(18), AT_RANDOM);
        write_volatile(stack_ptr.offset(19), random_ptr);
        write_volatile(stack_ptr.offset(20), AT_NULL);
        write_volatile(stack_ptr.offset(21), 0u64);

        // Random data (16 bytes) at adjusted_stack + 224
        let random_data = adjusted_stack as *mut u8;
        core::ptr::copy_nonoverlapping(b"\x12\x34\x56\x78\x9A\xBC\xDE\xF0\x0F\xED\xCB\xA9\x87\x65\x43\x21".as_ptr(), random_data.add(224), 16);
        
        // Program name string at adjusted_stack + 240
        core::ptr::copy_nonoverlapping(b"program\0".as_ptr(), random_data.add(240), 8);
    }

    unsafe {
        asm!(
            "cli",
            "push {ss}",
            "push {usp}",
            "push {rfl}",
            "push {cs}",
            "push {rip}",
            "mov ax, 0x23",
            "mov ds, ax",
            "mov es, ax",
            "swapgs",
            "xor ax, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ecx, 0xC0000100",
            "mov rax, r11",
            "mov rdx, r11",
            "shr rdx, 32",
            "wrmsr",
            "xor rax, rax",
            "xor rbx, rbx",
            "xor rcx, rcx",
            "xor rdx, rdx",
            "xor rsi, rsi",
            "xor rdi, rdi",
            "xor r8, r8",
            "xor r9, r9",
            "xor r10, r10",
            "xor r11, r11",
            "xor r12, r12",
            "xor r13, r13",
            "xor r14, r14",
            "xor r15, r15",
            "xor rbp, rbp",
            "iretq",
            rip = in(reg) entry_point,
            cs = in(reg) 0x1bu64,
            rfl = in(reg) 0x202u64,
            usp = in(reg) adjusted_stack,
            ss = in(reg) 0x23u64,
            in("r11") tls_base,
            options(noreturn)
        );
    }
}

/// Jump to entry point in userspace (Ring 3)
/// phdr_va, phnum, and phentsize are written to auxv (AT_PHDR, AT_PHNUM, AT_PHENT) so glibc can find program headers.
/// This function never returns
///
/// # Safety
/// This function constructs a stack frame and executes `iretq` to switch privilege levels.
/// It MUST be called with a valid userspace entry point and stack top.
/// CR3 should already be set to the correct process address space before calling this.
pub unsafe extern "C" fn jump_to_userspace(entry_point: u64, stack_top: u64, phdr_va: u64, phnum: u64, phentsize: u64) -> ! {
    let _pid = current_process_id().unwrap_or(0xFFFF);
    crate::serial::serial_printf(format_args!("[ELF] PID {} jumping to userspace at {:#x} with RSP {:#x} phdr={:#x}\n", _pid, entry_point, stack_top, phdr_va));
    
    // Safety check
    if entry_point == 0 {
        crate::serial::serial_print("ERROR: entry_point is ZERO! Cannot jump.\n");
        loop { core::arch::asm!("hlt"); }
    }

    // Verify entry point is in user space
    if entry_point >= USER_ADDR_MAX {
        serial::serial_print("ERROR: Entry point in kernel space!\n");
        loop { core::arch::asm!("hlt"); }
    }
    
    // System V ABI x86-64 stack alignment requirements:
    // - At program entry: RSP must be 16-byte aligned (RSP & 0xF == 0)
    // - Before CALL instruction: Stack arranged so (RSP+8) is 16-byte aligned
    //   (because CALL pushes return address, making RSP misaligned after)
    //
    // We're at program entry (not a function call), so RSP should be 16-byte aligned.
    // Stack layout at program start (System V ABI):
    //   [RSP+0]  = argc
    //   [RSP+8]  = argv[0] (or NULL if no args)
    //   [RSP+16] = argv[1] (or NULL as terminator)
    //   ...
    //   [RSP + 8*(argc+1)] = NULL (end of argv)
    //   [RSP + 8*(argc+2)] = envp[0] (or NULL)
    //   ...
    //   [RSP + 8*(argc+2+envc)] = NULL (end of envp)
    //   [RSP + ...] = auxv[0].a_type (auxiliary vector entries)
    //   [RSP + ...] = auxv[0].a_val
    //   ...
    //   [RSP + ...] = AT_NULL (0) to terminate auxv
    //   [RSP + ...] = 0 (value for AT_NULL)
    
    // For argc=0 with auxv so glibc works.
    // - 1 qword: argc = 0
    // - 1 qword: argv[0] = NULL (argv terminator)
    // - 1 qword: envp[0] = NULL (envp terminator)
    // - auxv: AT_PHDR(3), AT_PHENT(4), AT_PHNUM(5), AT_PAGESZ(6), AT_RANDOM(25), AT_NULL(0)
    // - 16 bytes for AT_RANDOM data
    // Total: 3 (argc/argv/envp) + 12 qwords (6 auxv entries × 2 qwords) + 2 (random data) = 17 quadwords = 136 bytes.
    // We subtract 144 bytes to keep 16-byte alignment.
    const AT_PAGESZ: u64 = 6;
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_RANDOM: u64 = 25;
    const AT_NULL: u64 = 0;
    // System V ABI for x86-64 at program entry specifies RSP is 16-byte aligned (RSP % 16 == 0).
    // Previous code was subtracting 8, which is only for function calls, not process entry.
    let adjusted_stack = (stack_top - 256) & !0xF;
    let program_ptr = adjusted_stack + 240;
    let random_ptr = adjusted_stack + 224;

    let _pid = current_process_id().unwrap_or(0xFFFF);
    crate::serial::serial_printf(format_args!("[ELF] PID {} jumping to userspace at {:#x} with RSP {:#x}\n", _pid, entry_point, adjusted_stack));

    // Always reload CR3 to flush the TLB. exec remaps code pages via
    // map_user_page_4kb but the TLB may still cache the old fork'd PTEs.
    // A CR3 write flushes all non-global TLB entries.
    //
    // Leer proc.fs_base antes del asm: tras `mov fs,0` hay que hacer wrmsr a IA32_FS_BASE
    // (en long mode el selector no limpia el MSR); tls_base==0 debe escribirse explícitamente.
    let tls_base: u64 = if let Some(pid) = current_process_id() {
        if let Some(proc) = get_process(pid) {
            let cr3 = proc.resources.lock().page_table_phys;
            unsafe { memory::set_cr3(cr3); }
            proc.fs_base
        } else { 0 }
    } else { 0 };

    // Write argc/argv/envp/auxv. Put AT_PHDR/AT_PHENT/AT_PHNUM first (some glibc inits use them early).
    unsafe {
        let stack_ptr = adjusted_stack as *mut u64;
        write_volatile(stack_ptr.offset(0), 1u64);         // argc = 1
        write_volatile(stack_ptr.offset(1), program_ptr); // argv[0] = "program"
        write_volatile(stack_ptr.offset(2), 0u64);         // argv[1] = NULL
        write_volatile(stack_ptr.offset(3), 0u64);         // envp[0] = NULL

        write_volatile(stack_ptr.offset(4), AT_PHDR);
        write_volatile(stack_ptr.offset(5), phdr_va);
        write_volatile(stack_ptr.offset(6), AT_PHENT);
        write_volatile(stack_ptr.offset(7), phentsize);
        write_volatile(stack_ptr.offset(8), AT_PHNUM);
        write_volatile(stack_ptr.offset(9), phnum);
        write_volatile(stack_ptr.offset(10), AT_PAGESZ);
        write_volatile(stack_ptr.offset(11), 4096u64);
        write_volatile(stack_ptr.offset(12), AT_RANDOM);
        write_volatile(stack_ptr.offset(13), random_ptr);
        write_volatile(stack_ptr.offset(14), AT_NULL);
        write_volatile(stack_ptr.offset(15), 0u64);

        // Random data (16 bytes) at adjusted_stack + 224
        let random_data = adjusted_stack as *mut u8;
        core::ptr::copy_nonoverlapping(b"\x12\x34\x56\x78\x9A\xBC\xDE\xF0\x0F\xED\xCB\xA9\x87\x65\x43\x21".as_ptr(), random_data.add(224), 16);
        
        // Program name string at adjusted_stack + 240
        core::ptr::copy_nonoverlapping(b"program\0".as_ptr(), random_data.add(240), 8);
    }

    // Construir el frame iretq directamente en el stack del kernel (por-CPU).
    // Evitamos un static mut compartido que causaría una carrera SMP cuando
    // dos CPUs ejecutan jump_to_userspace simultáneamente (exec concurrente).
    //
    // Diseño del frame en memoria (iretq hace pop de abajo arriba):
    //   [RSP+0]:  RIP       = entry_point  (primero en ser procesado por iretq)
    //   [RSP+8]:  CS        = 0x1b         (user code segment, DPL3)
    //   [RSP+16]: RFLAGS    = 0x202        (IF=1, bit reservado)
    //   [RSP+24]: RSP       = adjusted_stack
    //   [RSP+32]: SS        = 0x23         (user data segment, DPL3)
    //
    // Para construir este layout, hacemos PUSH en orden inverso al de pop
    // (SS primero → queda en la dirección más alta; RIP último → en la más baja).
    unsafe {
        asm!(
            "cli",
            // Construir frame en el stack del kernel (privado por CPU)
            "push {ss}",    // SS   = 0x23
            "push {usp}",   // RSP  = adjusted_stack
            "push {rfl}",   // RFLAGS = 0x202
            "push {cs}",    // CS   = 0x1b
            "push {rip}",   // RIP  = entry_point
            // Cargar selectores de segmento de usuario
            "mov ax, 0x23",
            "mov ds, ax",
            "mov es, ax",
            
            // CRITICAL: Al igual que en fork_child_trampoline, el kernel GS base 
            // debe protegerse en IA32_KERNEL_GS_BASE antes de tocar el selector GS.
            "swapgs",
            
            "xor ax, ax",
            "mov fs, ax",
            "mov gs, ax",
            // En modo largo, `mov fs, 0` no pone IA32_FS_BASE a 0: hay que usar wrmsr.
            // Si omitimos wrmsr con tls_base==0, queda el FS_BASE del proceso anterior
            // (p. ej. exec dinámico tras gui_service) y %fs: sigue apuntando al TCB viejo → #PF.
            // r11 = tls_base (0 si ld-musl debe instalar TLS).
            "mov ecx, 0xC0000100",
            "mov rax, r11",
            "mov rdx, r11",
            "shr rdx, 32",
            "wrmsr",
            // Limpiar todos los registros GP antes de entrar a userspace
            "xor rax, rax",
            "xor rbx, rbx",
            "xor rcx, rcx",
            "xor rdx, rdx",
            "xor rsi, rsi",
            "xor rdi, rdi",
            "xor r8, r8",
            "xor r9, r9",
            "xor r10, r10",
            "xor r11, r11",
            "xor r12, r12",
            "xor r13, r13",
            "xor r14, r14",
            "xor r15, r15",
            "xor rbp, rbp",
            "iretq",
            rip = in(reg) entry_point,
            cs  = in(reg) 0x1bu64,
            rfl = in(reg) 0x202u64,
            usp = in(reg) adjusted_stack,
            ss  = in(reg) 0x23u64,
            in("r11") tls_base,
            options(noreturn)
        );
    }
}