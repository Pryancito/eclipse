//! ELF Loader para cargar binarios en userspace.
//!
//! División kernel vs ld.so y rutas de spawn: ver `ELF_LOADING.md` en este crate.

use crate::process::{current_process_id, get_process, ProcessId};
use crate::filesystem;
use crate::memory;
use crate::serial;
use alloc::vec::Vec;
use core::arch::asm;
use core::ptr::write_volatile;

/// Trait to unify reading ELF data from both memory buffers and the filesystem.
pub trait ElfDataProvider {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, &'static str>;
    fn read_header(&self, buf: &mut [u8]) -> Result<(), &'static str> {
        let n = self.read_at(0, buf)?;
        if n < buf.len() {
            return Err("ELF: failed to read header part");
        }
        Ok(())
    }
    fn len(&self) -> usize;
}

pub struct SliceDataProvider<'a>(pub &'a [u8]);

impl<'a> ElfDataProvider for SliceDataProvider<'a> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, &'static str> {
        let offset = offset as usize;
        if offset >= self.0.len() {
            return Ok(0);
        }
        let len = core::cmp::min(buf.len(), self.0.len() - offset);
        buf[..len].copy_from_slice(&self.0[offset..offset + len]);
        Ok(len)
    }
    fn len(&self) -> usize {
        self.0.len()
    }
}

pub struct InodeDataProvider {
    pub inode: u32,
    pub cached_len: usize,
}

impl ElfDataProvider for InodeDataProvider {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, &'static str> {
        crate::filesystem::Filesystem::read_file_by_inode_at(self.inode, buf, offset)
    }
    fn len(&self) -> usize {
        self.cached_len
    }
}

/// Minimal envp strings injected into all freshly-spawned processes so that
/// bash/musl and other Linux-ABI programs can find binaries and terminals.
pub const MINIMAL_ENVP: &[&[u8]] = &[
    b"PATH=/bin:/usr/bin\0",
    b"HOME=/\0",
    b"TERM=xterm-256color\0",
    b"USER=root\0",
    b"SHELL=/bin/bash\0",
    b"LANG=C\0",
];

/// Number of entries in MINIMAL_ENVP.
pub const MINIMAL_ENVP_COUNT: usize = MINIMAL_ENVP.len();

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
// p_flags (ELF): R=4 W=2 X=1
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
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

const PHDR_MIN_SIZE: usize = core::mem::size_of::<Elf64ProgramHeader>();

fn assert_ph_table_in_bounds(
    elf_len: usize,
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
) -> Result<(), &'static str> {
    if ph_size < PHDR_MIN_SIZE {
        return Err("ELF: phentsize too small");
    }
    let bytes = ph_count
        .checked_mul(ph_size)
        .ok_or("ELF: ph table size overflow")?;
    let end = ph_offset
        .checked_add(bytes)
        .ok_or("ELF: ph table end overflow")?;
    if end > elf_len {
        return Err("ELF: program headers out of bounds");
    }
    Ok(())
}

fn program_header_at(
    provider: &dyn ElfDataProvider,
    ph_offset: usize,
    index: usize,
    ph_ent_size: usize,
) -> Result<Elf64ProgramHeader, &'static str> {
    if ph_ent_size < PHDR_MIN_SIZE {
        return Err("ELF: phentsize too small");
    }
    let idx_mul = index
        .checked_mul(ph_ent_size)
        .ok_or("ELF: ph table overflow")?;
    let base = ph_offset
        .checked_add(idx_mul)
        .ok_or("ELF: ph table overflow")?;
    
    let mut ph = Elf64ProgramHeader {
        p_type: 0, p_flags: 0, p_offset: 0, p_vaddr: 0, p_paddr: 0, p_filesz: 0, p_memsz: 0, p_align: 0
    };

    let ph_slice = unsafe {
        core::slice::from_raw_parts_mut(&mut ph as *mut _ as *mut u8, PHDR_MIN_SIZE)
    };

    let n = provider.read_at(base as u64, ph_slice)?;
    if n < PHDR_MIN_SIZE {
        return Err("ELF: failed to read program header");
    }
    Ok(ph)
}

fn r_sym(info: u64) -> usize {
    (info >> 32) as usize
}

fn r_type(info: u64) -> u32 {
    (info & 0xffff_ffff) as u32
}

fn va_to_file_off(
    provider: &dyn ElfDataProvider,
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
    vaddr: u64,
) -> Option<usize> {
    for i in 0..ph_count {
        let ph = program_header_at(provider, ph_offset, i, ph_size).ok()?;
        if ph.p_type == PT_LOAD && vaddr >= ph.p_vaddr && vaddr < ph.p_vaddr.saturating_add(ph.p_filesz) {
            let offset_in_seg = vaddr - ph.p_vaddr;
            return ph.p_offset.checked_add(offset_in_seg).map(|o| o as usize);
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
    provider: &dyn ElfDataProvider,
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
    load_bias: u64,
) -> u64 {
    for i in 0..ph_count {
        let Ok(ph) = program_header_at(provider, ph_offset, i, ph_size) else {
            break;
        };
        if ph.p_type == PT_LOAD {
            return ph.p_vaddr.wrapping_add(load_bias);
        }
    }
    0
}

fn extract_interp_path(
    provider: &dyn ElfDataProvider,
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
) -> Result<Option<alloc::string::String>, &'static str> {
    use alloc::string::String;
    use alloc::vec;
    for i in 0..ph_count {
        let ph = program_header_at(provider, ph_offset, i, ph_size)?;
        if ph.p_type != PT_INTERP {
            continue;
        }
        if ph.p_filesz > ph.p_memsz {
            return Err("ELF: PT_INTERP p_filesz > p_memsz");
        }
        if ph.p_filesz > 4096 {
            return Err("ELF: PT_INTERP path too long");
        }
        
        let mut bytes = vec![0u8; ph.p_filesz as usize];
        let n = provider.read_at(ph.p_offset, &mut bytes)?;
        if n < bytes.len() {
            return Err("ELF: failed to read PT_INTERP path");
        }

        let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let path = core::str::from_utf8(&bytes[..nul]).map_err(|_| "ELF: PT_INTERP not UTF-8")?;
        return Ok(Some(String::from(path)));
    }
    Ok(None)
}

fn has_pt_interp(provider: &dyn ElfDataProvider, ph_offset: usize, ph_count: usize, ph_size: usize) -> bool {
    extract_interp_path(provider, ph_offset, ph_count, ph_size)
        .ok()
        .flatten()
        .is_some()
}

fn validate_entry_for_load(
    provider: &dyn ElfDataProvider,
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
        let ph = program_header_at(provider, ph_offset, i, ph_size)?;
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
    provider: &dyn ElfDataProvider,
    ph_offset: usize,
    ph_count: usize,
    ph_size: usize,
    load_bias: u64,
    et_dyn: bool,
) -> Result<(u64, u32), &'static str> {
    let mut mapped_count: u32 = 0;
    let mut max_vaddr: u64 = 0;
    for i in 0..ph_count {
        let ph = program_header_at(provider, ph_offset, i, ph_size)?;
        if ph.p_type != PT_LOAD {
            continue;
        }
        if ph.p_filesz > ph.p_memsz {
            return Err("ELF: PT_LOAD p_filesz > p_memsz");
        }
        if ph.p_align > 0 && !ph.p_align.is_power_of_two() {
            return Err("ELF: invalid segment alignment (not power of two)");
        }
        if ph.p_align > 1 && (ph.p_vaddr % ph.p_align != ph.p_offset % ph.p_align) {
            return Err("ELF: vaddr and offset alignment mismatch");
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
            "[load_elf] segment {}: vaddr=0x{:x} filesz=0x{:x} memsz=0x{:x} flags=0x{:x}\n",
            i, vaddr_start, ph.p_filesz, ph.p_memsz, ph.p_flags
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
            unsafe { core::ptr::write_bytes(kptr, 0, 0x1000) };
            if allocated_new {
                mapped_count = mapped_count.saturating_add(1);
            }
            // Permisos alineados con p_flags (NX en segmentos sin PF_X); coherente con mprotect/mmap.
            let pte = crate::memory::linux_prot_to_leaf_pte_bits(ph_flags_to_linux_prot(ph.p_flags));
            crate::memory::map_user_page_4kb(page_table_phys, current_vaddr, phys, pte);
            if file_size > 0 {
                let page_vaddr_start = current_vaddr;
                let page_vaddr_end = current_vaddr + 0x1000;
                let intersect_start = core::cmp::max(vaddr_start, page_vaddr_start);
                let intersect_end = core::cmp::min(vaddr_start + file_size, page_vaddr_end);
                if intersect_start < intersect_end {
                    let copy_size = (intersect_end - intersect_start) as usize;
                    let in_file_segment_offset = intersect_start - vaddr_start;
                    let file_src_start = file_start_offset + in_file_segment_offset;
                    let in_page_offset = (intersect_start - page_vaddr_start) as usize;
                    
                    let dst = unsafe { kptr.add(in_page_offset) };
                    let dst_slice = unsafe { core::slice::from_raw_parts_mut(dst, copy_size) };
                    
                    let n = provider.read_at(file_src_start, dst_slice)?;
                    if n < copy_size {
                        return Err("ELF: failed to read segment data from provider");
                    }
                }
            }
            current_vaddr += 0x1000;
        }
    }
    Ok((max_vaddr, mapped_count))
}

/// Convierte `p_flags` de un `PT_LOAD` a máscara Linux `PROT_*` para [`memory::linux_prot_to_leaf_pte_bits`].
fn ph_flags_to_linux_prot(p_flags: u32) -> u64 {
    let mut p = 0u64;
    if (p_flags & PF_R) != 0 {
        p |= 1;
    } // PROT_READ
    if (p_flags & PF_W) != 0 {
        p |= 2;
    } // PROT_WRITE
    if (p_flags & PF_X) != 0 {
        p |= 4;
    } // PROT_EXEC
    if p == 0 {
        p = 1;
    }
    p
}

fn tls_setup_after_load(
    page_table_phys: u64,
    provider: &dyn ElfDataProvider,
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
                    crate::memory::linux_prot_to_leaf_pte_bits(3), // RW, NX
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
                
                let dst = unsafe { kptr.add(page_off) };
                let dst_slice = unsafe { core::slice::from_raw_parts_mut(dst, chunk) };
                if let Err(e) = provider.read_at(tls_file_offset + src_off as u64, dst_slice) {
                    serial::serial_printf(format_args!("[load_elf] TLS read error: {}\n", e));
                    break;
                }
                
                src_off += chunk;
            } else {
                break;
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
    provider: &dyn ElfDataProvider,
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
        let Ok(ph) = program_header_at(provider, ph_offset, i, ph_size) else {
            break;
        };
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


fn phdr_va_biased(provider: &dyn ElfDataProvider, load_bias: u64, et_dyn: bool) -> Result<(u64, u64, u64), &'static str> {
    let mut header = Elf64Header {
        e_ident: [0; 16], e_type: 0, e_machine: 0, e_version: 0, e_entry: 0, e_phoff: 0, e_shoff: 0,
        e_flags: 0, e_ehsize: 0, e_phentsize: 0, e_phnum: 0, e_shentsize: 0, e_shnum: 0, e_shstrndx: 0
    };
    provider.read_header(unsafe { core::slice::from_raw_parts_mut(&mut header as *mut _ as *mut u8, core::mem::size_of::<Elf64Header>()) })?;

    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    assert_ph_table_in_bounds(provider.len(), ph_offset, ph_count, ph_size)?;
    let mut first_load_vaddr = None;
    for i in 0..ph_count {
        let ph = program_header_at(provider, ph_offset, i, ph_size)?;
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

fn load_elf_dynamic_pair(page_table_phys: u64, main_provider: &dyn ElfDataProvider, interp_path: &str) -> Result<ExecLoadResult, &'static str> {
    serial::serial_printf(format_args!(
        "[load_elf] dynamic: loading interpreter \"{}\"\n",
        interp_path
    ));
    
    let interp_inode =
        filesystem::Filesystem::lookup_path_resolve_file_inode(interp_path.trim_start_matches('/'))?;
    let interp_len = filesystem::Filesystem::content_len_by_inode(interp_inode)?;
    let interp_provider = InodeDataProvider { inode: interp_inode, cached_len: interp_len };

    let mut main_h = Elf64Header {
        e_ident: [0; 16], e_type: 0, e_machine: 0, e_version: 0, e_entry: 0, e_phoff: 0, e_shoff: 0,
        e_flags: 0, e_ehsize: 0, e_phentsize: 0, e_phnum: 0, e_shentsize: 0, e_shnum: 0, e_shstrndx: 0
    };
    main_provider.read_header(unsafe { core::slice::from_raw_parts_mut(&mut main_h as *mut _ as *mut u8, core::mem::size_of::<Elf64Header>()) })?;

    let mut interp_h = Elf64Header {
        e_ident: [0; 16], e_type: 0, e_machine: 0, e_version: 0, e_entry: 0, e_phoff: 0, e_shoff: 0,
        e_flags: 0, e_ehsize: 0, e_phentsize: 0, e_phnum: 0, e_shentsize: 0, e_shnum: 0, e_shstrndx: 0
    };
    interp_provider.read_header(unsafe { core::slice::from_raw_parts_mut(&mut interp_h as *mut _ as *mut u8, core::mem::size_of::<Elf64Header>()) })?;

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
    assert_ph_table_in_bounds(main_provider.len(), main_ph_off, main_ph_count, main_ph_size)?;
    assert_ph_table_in_bounds(interp_provider.len(), interp_ph_off, interp_ph_count, interp_ph_size)?;

    if has_pt_interp(&interp_provider, interp_ph_off, interp_ph_count, interp_ph_size) {
        return Err("ELF: nested PT_INTERP in interpreter");
    }

    let main_et_dyn = main_h.e_type == ET_DYN;
    let main_bias = if main_et_dyn {
        DYNAMIC_MAIN_LOAD_BIAS
    } else {
        0u64
    };
    validate_entry_for_load(
        main_provider,
        &main_h,
        main_ph_off,
        main_ph_count,
        main_ph_size,
        main_bias,
        main_et_dyn,
    )?;

    let interp_et_dyn = interp_h.e_type == ET_DYN;
    validate_entry_for_load(
        &interp_provider,
        &interp_h,
        interp_ph_off,
        interp_ph_count,
        interp_ph_size,
        0,
        interp_et_dyn,
    )?;

    let (max_v_main, mut mapped) = map_pt_load_segments(
        page_table_phys,
        main_provider,
        main_ph_off,
        main_ph_count,
        main_ph_size,
        main_bias,
        main_et_dyn,
    )?;
    let max_v_main_al = (max_v_main + 0xFFF) & !0xFFF;
    let main_end = max_v_main_al.saturating_add(0x1000);

    let base_above_main = main_end.saturating_add(DYNAMIC_INTERP_GAP);
    let step = 0x1_0000_0000u64;
    let interp_bias = (base_above_main.saturating_add(step - 1) / step).saturating_mul(step);
    
    let (max_v_interp, mapped_i) = map_pt_load_segments(
        page_table_phys,
        &interp_provider,
        interp_ph_off,
        interp_ph_count,
        interp_ph_size,
        interp_bias,
        interp_et_dyn,
    )?;
    mapped = mapped.saturating_add(mapped_i);

    let (phdr_va, phnum, phent) = phdr_va_biased(main_provider, main_bias, main_et_dyn)?;

    let main_entry = if main_et_dyn { main_bias.wrapping_add(main_h.e_entry) } else { main_h.e_entry };
    let interp_entry = if interp_et_dyn { interp_bias.wrapping_add(interp_h.e_entry) } else { interp_h.e_entry };

    let mut res = ExecLoadResult {
        entry_point: interp_entry,
        max_vaddr: max_v_interp,
        phdr_va,
        phnum,
        phentsize: phent,
        segment_frames: mapped as u64,
        tls_base: 0,
        dynamic_linker: Some((interp_bias, main_entry)),
        loaded_vma_ranges: [(0, 0), (0, 0)],
        loaded_vma_count: 0,
    };
    res.loaded_vma_ranges[0] = (min_load_vaddr(main_provider, main_ph_off, main_ph_count, main_ph_size, main_bias), max_v_main);
    res.loaded_vma_ranges[1] = (min_load_vaddr(&interp_provider, interp_ph_off, interp_ph_count, interp_ph_size, interp_bias), max_v_interp);
    res.loaded_vma_count = 2;

    Ok(res)
}


/// Cargar los segmentos del ELF en el espacio de direcciones especificado.
pub fn load_elf_into_space(page_table_phys: u64, provider: &dyn ElfDataProvider) -> Result<ExecLoadResult, &'static str> {
    serial::serial_printf(format_args!(
        "[load_elf] start len={} cr3=0x{:x}\n",
        provider.len(),
        page_table_phys
    ));
    
    let mut header = Elf64Header {
        e_ident: [0; 16], e_type: 0, e_machine: 0, e_version: 0, e_entry: 0, e_phoff: 0, e_shoff: 0,
        e_flags: 0, e_ehsize: 0, e_phentsize: 0, e_phnum: 0, e_shentsize: 0, e_shnum: 0, e_shstrndx: 0
    };
    provider.read_header(unsafe { core::slice::from_raw_parts_mut(&mut header as *mut _ as *mut u8, core::mem::size_of::<Elf64Header>()) })?;

    if &header.e_ident[0..4] != &ELF_MAGIC {
        return Err("ELF: Invalid magic number");
    }
    if header.e_ident[4] != 2 {
        return Err("ELF: not 64-bit");
    }
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    assert_ph_table_in_bounds(provider.len(), ph_offset, ph_count, ph_size)?;

    if let Some(interp_path) = extract_interp_path(provider, ph_offset, ph_count, ph_size)? {
        return load_elf_dynamic_pair(page_table_phys, provider, interp_path.as_str());
    }

    let et_dyn = header.e_type == ET_DYN;
    let load_bias = if et_dyn {
        let rnd = crate::cpu::get_random_u64();
        // Randomize bias in [1GB, 3GB) range, 2MB aligned.
        0x4000_0000u64 + (rnd % 0x8000_0000u64 & !0x1F_FFFFu64)
    } else {
        0u64
    };
    if header.e_type != ET_EXEC && !et_dyn {
        return Err("ELF: not ET_EXEC/ET_DYN");
    }
    validate_entry_for_load(
        provider,
        &header,
        ph_offset,
        ph_count,
        ph_size,
        load_bias,
        et_dyn,
    )?;

    let (has_tls, tf, tm, tfo, ta) = collect_tls_phdr(provider, ph_offset, ph_count, ph_size);
    let (max_v, mut mapped) = map_pt_load_segments(
        page_table_phys,
        provider,
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
        provider,
        max_vaddr_aligned,
        tf,
        tm,
        tfo,
        ta,
        has_tls,
        &mut mapped,
    );

    let (phdr_va, phnum, phentsize) = phdr_va_biased(provider, load_bias, et_dyn)?;

    serial::serial_printf(format_args!(
        "[load_elf] successfully loaded: entry=0x{:x} max_v=0x{:x} mapped_pages={}\n",
        entry_point, max_vaddr_aligned, mapped
    ));

    let min_vaddr = min_load_vaddr(provider, ph_offset, ph_count, ph_size, load_bias);

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
            // Zero the frame before mapping: freed frames returned by unmap_user_range
            // may contain stale data from previous processes.  Leaving them unzeroed
            // lets old stack values (including non-canonical pointers) bleed into the
            // new process's stack, corrupting callee-saved registers on function return
            // and causing a #GP fault.  All other alloc_phys_frame_for_anon_mmap call
            // sites (ELF segments, TLS, sys_mmap) already zero the frame; stack pages
            // must do the same.
            unsafe {
                core::ptr::write_bytes(
                    crate::memory::phys_to_virt(phys) as *mut u8,
                    0,
                    0x1000,
                );
            }
            let offset = (i as u64) * 0x1000;
            crate::memory::map_user_page_4kb(
                page_table_phys,
                stack_base + offset,
                phys,
                crate::memory::linux_prot_to_leaf_pte_bits(3), // RW, NX
            );
        } else {
            serial::serial_printf(format_args!("[setup_stack] alloc failed at page {}\n", i));
            return Err("Failed to allocate 4KB anonymous frame for user stack");
        }
    }
    serial::serial_print("[setup_stack] done\n");
    Ok(stack_base + stack_size as u64)
}

/// Cargar binario ELF en memoria y crear proceso (desde un slice en memoria)
pub fn load_elf(elf_data: &[u8]) -> Option<ProcessId> {
    load_elf_provider(&SliceDataProvider(elf_data))
}

/// Cargar binario ELF desde una ruta en el filesystem (streaming, eficiente en memoria)
pub fn load_elf_path(path: &str) -> Option<ProcessId> {
    let inode = match filesystem::Filesystem::lookup_path_resolve_file_inode(path.trim_start_matches('/')) {
        Ok(i) => i,
        Err(_) => return None,
    };
    let len = match filesystem::Filesystem::content_len_by_inode(inode) {
        Ok(l) => l,
        Err(_) => return None,
    };
    let provider = InodeDataProvider { inode, cached_len: len };
    load_elf_provider(&provider)
}

fn load_elf_provider(provider: &dyn ElfDataProvider) -> Option<ProcessId> {
    // Create the page table for this process ONCE and reuse it throughout.
    let cr3 = crate::memory::create_process_paging();

    let loaded = match load_elf_into_space(cr3, provider) {
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
    let stack_size = 0x100000;  // 1MB

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
    // Slot index comes from the PID→slot map (not `pid` as table index).
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = crate::process::PROCESS_TABLE.lock();
        if let Some(slot) = crate::ipc::pid_to_slot_fast(pid) {
            if let Some(p) = table[slot].as_mut() {
                if p.id == pid {
                    let mut proc = p.proc.lock();
                    proc.mem_frames += loaded.segment_frames;
                    if loaded.tls_base != 0 {
                        p.fs_base = loaded.tls_base;
                    }
                    proc.dynamic_linker_aux = loaded.dynamic_linker;
                }
            }
        }
    });

    crate::fd::fd_init_stdio(pid);

    Some(pid)
}

fn load_info(provider: &dyn ElfDataProvider) -> Result<(u64, usize, usize, usize), &'static str> {
    let mut header = Elf64Header {
        e_ident: [0; 16], e_type: 0, e_machine: 0, e_version: 0, e_entry: 0, e_phoff: 0, e_shoff: 0,
        e_flags: 0, e_ehsize: 0, e_phentsize: 0, e_phnum: 0, e_shentsize: 0, e_shnum: 0, e_shstrndx: 0
    };
    provider.read_header(unsafe { core::slice::from_raw_parts_mut(&mut header as *mut _ as *mut u8, core::mem::size_of::<Elf64Header>()) })?;
    
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
pub fn get_elf_phdr_info(provider: &dyn ElfDataProvider) -> Result<(u64, u64, u64), &'static str> {
    let mut header = Elf64Header {
        e_ident: [0; 16], e_type: 0, e_machine: 0, e_version: 0, e_entry: 0, e_phoff: 0, e_shoff: 0,
        e_flags: 0, e_ehsize: 0, e_phentsize: 0, e_phnum: 0, e_shentsize: 0, e_shnum: 0, e_shstrndx: 0
    };
    provider.read_header(unsafe { core::slice::from_raw_parts_mut(&mut header as *mut _ as *mut u8, core::mem::size_of::<Elf64Header>()) })?;

    if &header.e_ident[0..4] != &ELF_MAGIC {
        return Err("ELF: invalid magic");
    }
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    assert_ph_table_in_bounds(provider.len(), ph_offset, ph_count, ph_size)?;
    let mut first_load_vaddr = None;
    for i in 0..ph_count {
        let ph = program_header_at(provider, ph_offset, i, ph_size)?;
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
    replace_process_image_provider(pid, &SliceDataProvider(elf_data))
}

pub fn replace_process_image_path(pid: ProcessId, path: &str) -> Result<ExecLoadResult, &'static str> {
    let inode = filesystem::Filesystem::lookup_path_resolve_file_inode(path.trim_start_matches('/'))?;
    let len = filesystem::Filesystem::content_len_by_inode(inode)?;
    let provider = InodeDataProvider { inode, cached_len: len };
    replace_process_image_provider(pid, &provider)
}

fn replace_process_image_provider(pid: ProcessId, provider: &dyn ElfDataProvider) -> Result<ExecLoadResult, &'static str> {
    serial::serial_printf(format_args!("[exec] replace_process_image for PID {}\n", pid));
    load_elf_into_space(crate::memory::get_cr3(), provider)
}

/// Maximum bytes in a kernel process name (16 chars + NUL = 17, padded to 20 for alignment).
const MAX_PROCESS_NAME_LEN: usize = 16;
/// Buffer size for argv0 fallback (name + NUL, rounded up).
const ARGV0_BUF_LEN: usize = 20;

/// Build the full argv list for a process from its pending args registered by the parent via
/// `set_child_args` (syscall 542).  Each returned `Vec<u8>` is a NUL-terminated string.
///
/// If no pending args exist (e.g. the process was spawned without `set_child_args`), the list
/// falls back to a single argv[0] built from the kernel process name.
fn build_argv_from_pending(pid: crate::process::ProcessId) -> Vec<Vec<u8>> {
    // Read the NUL-separated pending args registered by the parent.
    let mut pending_buf = [0u8; 4096];
    let n = crate::process::copy_pending_process_args(pid, &mut pending_buf);

    if n > 0 {
        // Parse "arg0\0arg1\0arg2\0..." into individual NUL-terminated byte vectors.
        let args: Vec<Vec<u8>> = pending_buf[..n]
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| {
                let mut v = s.to_vec();
                v.push(0); // ensure NUL-terminated
                v
            })
            .collect();
        if !args.is_empty() {
            return args;
        }
    }

    // Fallback: use process name as argv[0].
    let mut argv0 = [0u8; ARGV0_BUF_LEN];
    let argv0_len = if let Some(p) = get_process(pid) {
        let proc = p.proc.lock();
        let n = proc.name.iter().position(|&b| b == 0).unwrap_or(MAX_PROCESS_NAME_LEN).min(MAX_PROCESS_NAME_LEN);
        argv0[..n].copy_from_slice(&proc.name[..n]);
        argv0[n] = 0;
        n + 1
    } else {
        argv0[..8].copy_from_slice(b"program\0");
        8
    };
    let mut v = argv0[..argv0_len].to_vec();
    if v.last() != Some(&0) { v.push(0); }
    let mut result = Vec::with_capacity(1);
    result.push(v);
    result
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
    tls_arg: u64,
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

    // Build full argv from pending args registered by parent (set_child_args syscall).
    let argv_strings = build_argv_from_pending(_pid as crate::process::ProcessId);
    let argc = argv_strings.len();

    let mut str_total: usize = 0;
    for s in &argv_strings { str_total += s.len(); }
    for e in MINIMAL_ENVP { str_total += e.len(); }
    str_total += 16; // AT_RANDOM
    let str_area_size = (str_total + 15) & !15usize;

    const AUXV_ENTRIES: usize = 8; // AT_PHDR,AT_PHENT,AT_PHNUM,AT_PAGESZ,AT_BASE,AT_FLAGS,AT_ENTRY,AT_RANDOM
    let table_slots = 1 + (argc + 1) + (MINIMAL_ENVP_COUNT + 1) + (AUXV_ENTRIES + 1) * 2;
    let table_bytes = table_slots * 8;

    let strings_base_raw = (stack_top as usize).wrapping_sub(str_area_size);
    let rsp_raw = strings_base_raw.wrapping_sub(table_bytes);
    let adjusted_stack = (rsp_raw & !0xF) as u64;
    let strings_base = adjusted_stack + table_bytes as u64;

    let mut tls_msr = tls_arg;
    let (at_base, at_entry) = if let Some(pid) = current_process_id() {
        if let Some(p) = get_process(pid) {
            unsafe {
                memory::set_cr3(p.proc.lock().resources.lock().page_table_phys);
            }
            if tls_msr == 0 {
                tls_msr = p.fs_base;
            }
            let aux = p.proc.lock().dynamic_linker_aux;
            match aux {
                Some((b, e)) => (b, e),
                None => {
                    crate::serial::serial_print("ERROR: jump_to_userspace_dynamic_linker without dynamic_linker_aux\n");
                    loop {
                        core::arch::asm!("hlt");
                    }
                }
            }
        } else {
            (0u64, 0u64)
        }
    } else {
        (0u64, 0u64)
    };

    unsafe {
        let mut str_off: u64 = strings_base;

        // Write all argv strings and collect their pointers.
        let mut argv_ptrs = Vec::with_capacity(argc);
        for s in &argv_strings {
            argv_ptrs.push(str_off);
            core::ptr::copy_nonoverlapping(s.as_ptr(), str_off as *mut u8, s.len());
            str_off += s.len() as u64;
        }

        let mut env_ptrs = [0u64; MINIMAL_ENVP_COUNT];
        for (i, e) in MINIMAL_ENVP.iter().enumerate() {
            env_ptrs[i] = str_off;
            core::ptr::copy_nonoverlapping(e.as_ptr(), str_off as *mut u8, e.len());
            str_off += e.len() as u64;
        }

        let random_ptr = str_off;
        core::ptr::copy_nonoverlapping(
            b"\x12\x34\x56\x78\x9A\xBC\xDE\xF0\x0F\xED\xCB\xA9\x87\x65\x43\x21".as_ptr(),
            str_off as *mut u8, 16,
        );

        let table = adjusted_stack as *mut u64;
        let mut i: isize = 0;
        write_volatile(table.offset(i), argc as u64); i += 1;
        for p in &argv_ptrs { write_volatile(table.offset(i), *p); i += 1; }
        write_volatile(table.offset(i), 0u64); i += 1;
        for ep in env_ptrs.iter() { write_volatile(table.offset(i), *ep); i += 1; }
        write_volatile(table.offset(i), 0u64); i += 1;
        write_volatile(table.offset(i), AT_PHDR);   i += 1;
        write_volatile(table.offset(i), phdr_va);   i += 1;
        write_volatile(table.offset(i), AT_PHENT);  i += 1;
        write_volatile(table.offset(i), phentsize); i += 1;
        write_volatile(table.offset(i), AT_PHNUM);  i += 1;
        write_volatile(table.offset(i), phnum);     i += 1;
        write_volatile(table.offset(i), AT_PAGESZ); i += 1;
        write_volatile(table.offset(i), 4096u64);   i += 1;
        write_volatile(table.offset(i), AT_BASE);   i += 1;
        write_volatile(table.offset(i), at_base);   i += 1;
        write_volatile(table.offset(i), AT_FLAGS);  i += 1;
        write_volatile(table.offset(i), 0u64);      i += 1;
        write_volatile(table.offset(i), AT_ENTRY);  i += 1;
        write_volatile(table.offset(i), at_entry);  i += 1;
        write_volatile(table.offset(i), AT_RANDOM); i += 1;
        write_volatile(table.offset(i), random_ptr); i += 1;
        write_volatile(table.offset(i), AT_NULL);   i += 1;
        write_volatile(table.offset(i), 0u64);
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
            // FS: no cargar el selector antes de WRMSR(IA32_FS_BASE); ver fork_child_trampoline.
            "mov ecx, 0xC0000100",
            "mov eax, r11d",
            "mov rdx, r11",
            "shr rdx, 32",
            "wrmsr",
            "mov ax, 0x23",
            "mov gs, ax",
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
            in("r11") tls_msr,
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
///
/// `tls_arg` is the 6th SysV argument (register `r9` when entered via `switch_context`):
/// it must match `Process.context.r9` / `fs_base` at process creation. Call sites that
/// invoke this directly from Rust (e.g. `sys_exec`) must pass `ExecLoadResult.tls_base`
/// explicitly so `IA32_FS_BASE` is not derived from a stale `r9`.
pub unsafe extern "C" fn jump_to_userspace(
    entry_point: u64,
    stack_top: u64,
    phdr_va: u64,
    phnum: u64,
    phentsize: u64,
    tls_arg: u64,
) -> ! {
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
    
    const AT_PAGESZ: u64 = 6;
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_BASE: u64 = 7;
    const AT_ENTRY: u64 = 9;
    const AT_RANDOM: u64 = 25;
    const AT_NULL: u64 = 0;

    // Build full argv from pending args registered by parent (set_child_args syscall).
    let argv_strings = build_argv_from_pending(_pid as crate::process::ProcessId);
    let argc = argv_strings.len();

    // Calculate total string bytes needed.
    let mut str_total: usize = 0;
    for s in &argv_strings { str_total += s.len(); }
    for e in MINIMAL_ENVP { str_total += e.len(); }
    str_total += 16; // AT_RANDOM data
    let str_area_size = (str_total + 15) & !15usize;

    // Number of 8-byte slots: 1(argc) + argc+1(argv+NULL) + MINIMAL_ENVP_COUNT+1(envp+NULL) + 2*(7 auxv + AT_NULL)
    const AUXV_ENTRIES: usize = 7; // AT_PHDR,AT_PHENT,AT_PHNUM,AT_PAGESZ,AT_BASE,AT_ENTRY,AT_RANDOM
    let table_slots = 1 + (argc + 1) + (MINIMAL_ENVP_COUNT + 1) + (AUXV_ENTRIES + 1) * 2;
    let table_bytes = table_slots * 8;

    // RSP: place table right below the strings area, aligned to 16.
    let strings_base = (stack_top as usize).wrapping_sub(str_area_size);
    let rsp_raw = strings_base.wrapping_sub(table_bytes);
    let adjusted_stack = (rsp_raw & !0xF) as u64;
    let strings_base = adjusted_stack + table_bytes as u64;

    crate::serial::serial_printf(format_args!("[ELF] PID {} jumping to userspace at {:#x} with RSP {:#x}\n", _pid, entry_point, adjusted_stack));

    // Reload CR3; resolve TLS for WRMSR(IA32_FS_BASE). Prefer `tls_arg` (R9 from PCB on
    // first schedule); if zero, fall back to `proc.fs_base` so we never overwrite the
    // scheduler's WRMSR with 0 when the clone of `Process` or `current_process_id` is wrong.
    let mut tls_msr = tls_arg;
    let mut proc_fs_base = 0u64;
    if let Some(pid) = current_process_id() {
        if let Some(proc) = get_process(pid) {
            let cr3 = proc.proc.lock().resources.lock().page_table_phys;
            unsafe { memory::set_cr3(cr3); }
            if tls_msr == 0 {
                tls_msr = proc.fs_base;
            }
            proc_fs_base = proc.fs_base;
        }
    }
    crate::serial::serial_printf(format_args!(
        "[ELF] PID {} tls_arg={:#x} proc.fs_base={:#x} tls_msr(wrmsr)={:#x}\n",
        _pid, tls_arg, proc_fs_base, tls_msr
    ));

    // Write strings into user stack.
    unsafe {
        let mut str_off: u64 = strings_base;

        // Write all argv strings and collect their pointers.
        let mut argv_ptrs = Vec::with_capacity(argc);
        for s in &argv_strings {
            argv_ptrs.push(str_off);
            core::ptr::copy_nonoverlapping(s.as_ptr(), str_off as *mut u8, s.len());
            str_off += s.len() as u64;
        }

        // envp strings
        let mut env_ptrs = [0u64; MINIMAL_ENVP_COUNT];
        for (i, e) in MINIMAL_ENVP.iter().enumerate() {
            env_ptrs[i] = str_off;
            core::ptr::copy_nonoverlapping(e.as_ptr(), str_off as *mut u8, e.len());
            str_off += e.len() as u64;
        }

        // AT_RANDOM data (16 bytes)
        let random_ptr = str_off;
        core::ptr::copy_nonoverlapping(
            b"\x12\x34\x56\x78\x9A\xBC\xDE\xF0\x0F\xED\xCB\xA9\x87\x65\x43\x21".as_ptr(),
            str_off as *mut u8, 16,
        );

        // Write argc/argv/envp/auxv table.
        let table = adjusted_stack as *mut u64;
        let mut i: isize = 0;
        write_volatile(table.offset(i), argc as u64); i += 1;
        for p in &argv_ptrs { write_volatile(table.offset(i), *p); i += 1; }
        write_volatile(table.offset(i), 0u64); i += 1;       // argv NULL
        for ep in env_ptrs.iter() {
            write_volatile(table.offset(i), *ep); i += 1;
        }
        write_volatile(table.offset(i), 0u64); i += 1;       // envp NULL
        // auxv
        write_volatile(table.offset(i), AT_PHDR);   i += 1;
        write_volatile(table.offset(i), phdr_va);   i += 1;
        write_volatile(table.offset(i), AT_PHENT);  i += 1;
        write_volatile(table.offset(i), phentsize); i += 1;
        write_volatile(table.offset(i), AT_PHNUM);  i += 1;
        write_volatile(table.offset(i), phnum);     i += 1;
        write_volatile(table.offset(i), AT_PAGESZ); i += 1;
        write_volatile(table.offset(i), 4096u64);   i += 1;
        write_volatile(table.offset(i), AT_BASE);   i += 1;
        write_volatile(table.offset(i), 0u64);      i += 1;
        write_volatile(table.offset(i), AT_ENTRY);  i += 1;
        write_volatile(table.offset(i), entry_point); i += 1;
        write_volatile(table.offset(i), AT_RANDOM); i += 1;
        write_volatile(table.offset(i), random_ptr); i += 1;
        write_volatile(table.offset(i), AT_NULL);   i += 1;
        write_volatile(table.offset(i), 0u64);
    }

    // Build iretq frame and jump to userspace.
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
            // FS: no cargar el selector antes de WRMSR(IA32_FS_BASE); ver fork_child_trampoline.
            "mov ecx, 0xC0000100",
            "mov eax, r11d",
            "mov rdx, r11",
            "shr rdx, 32",
            "wrmsr",
            "mov ax, 0x23",
            "mov gs, ax",
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
            in("r11") tls_msr,
            options(noreturn)
        );
    }
}
/// Jump to userspace with explicit argv and envp (used by execve syscall).
/// Must be called AFTER replace_process_image has reloaded the ELF and
/// AFTER setup_user_stack has mapped the new stack region.
///
/// `argv_strings` and `envp_strings` are null-terminated byte slices stored in kernel memory.
/// `res` is the ExecLoadResult from replace_process_image.
/// `tls_base` is the new TLS base (0 if dynamic linker will install TLS).
pub unsafe fn jump_to_userspace_with_argv_envp(
    res: &ExecLoadResult,
    stack_top: u64,
    argv_strings: &[alloc::vec::Vec<u8>],
    envp_strings: &[alloc::vec::Vec<u8>],
    tls_base: u64,
) -> ! {
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_PAGESZ: u64 = 6;
    const AT_BASE: u64 = 7;
    const AT_FLAGS: u64 = 8;
    const AT_ENTRY: u64 = 9;
    const AT_RANDOM: u64 = 25;
    const AT_NULL: u64 = 0;

    let argc = argv_strings.len();
    let envc = envp_strings.len();

    // Calculate string bytes needed (each is already null-terminated).
    let mut str_bytes: usize = 16; // AT_RANDOM data
    for s in argv_strings.iter().chain(envp_strings.iter()) {
        str_bytes += s.len();
    }
    let str_area = (str_bytes + 15) & !15usize;

    // Auxv entry count (type+val pairs): AT_PHDR,AT_PHENT,AT_PHNUM,AT_PAGESZ,AT_RANDOM,
    // optionally AT_BASE,AT_FLAGS,AT_ENTRY, then AT_NULL.
    let has_interp = res.dynamic_linker.is_some();
    let auxv_count = if has_interp { 8 + 1 } else { 7 + 1 }; // +1 for AT_NULL

    // Table slots: 1(argc) + argc+1(argv) + envc+1(envp) + auxv_count*2
    let table_slots = 1 + (argc + 1) + (envc + 1) + auxv_count * 2;
    let table_bytes = table_slots * 8;

    // Strings sit above the table.
    let strings_base = (stack_top as usize).wrapping_sub(str_area);
    let rsp_raw = strings_base.wrapping_sub(table_bytes);
    
    // ASLR: Randomize stack offset by up to 4KB (16-byte aligned)
    let stack_jitter = (crate::cpu::get_random_u64() % 4096) & !0xF;
    let rsp = ((rsp_raw as u64 - stack_jitter) & !0xF) as u64;
    let strings_base = rsp + table_bytes as u64;

    // Switch CR3 to the (already-replaced) user page table.
    if let Some(pid) = current_process_id() {
        if let Some(proc) = get_process(pid) {
            let cr3 = proc.proc.lock().resources.lock().page_table_phys;
            memory::set_cr3(cr3);
        }
    }

    // Write strings and collect pointers.
    let mut str_off: u64 = strings_base;
    let mut argv_ptrs = alloc::vec::Vec::with_capacity(argc);
    for s in argv_strings {
        argv_ptrs.push(str_off);
        core::ptr::copy_nonoverlapping(s.as_ptr(), str_off as *mut u8, s.len());
        str_off += s.len() as u64;
    }
    let mut envp_ptrs = alloc::vec::Vec::with_capacity(envc);
    for s in envp_strings {
        envp_ptrs.push(str_off);
        core::ptr::copy_nonoverlapping(s.as_ptr(), str_off as *mut u8, s.len());
        str_off += s.len() as u64;
    }
    let random_ptr = str_off;
    core::ptr::copy_nonoverlapping(
        b"\x12\x34\x56\x78\x9A\xBC\xDE\xF0\x0F\xED\xCB\xA9\x87\x65\x43\x21".as_ptr(),
        str_off as *mut u8, 16,
    );

    // Write table.
    let table = rsp as *mut u64;
    let mut idx: isize = 0;
    write_volatile(table.offset(idx), argc as u64); idx += 1;
    for p in &argv_ptrs { write_volatile(table.offset(idx), *p); idx += 1; }
    write_volatile(table.offset(idx), 0u64); idx += 1; // argv NULL
    for p in &envp_ptrs { write_volatile(table.offset(idx), *p); idx += 1; }
    write_volatile(table.offset(idx), 0u64); idx += 1; // envp NULL
    // auxv
    write_volatile(table.offset(idx), AT_PHDR);       idx += 1;
    write_volatile(table.offset(idx), res.phdr_va);   idx += 1;
    write_volatile(table.offset(idx), AT_PHENT);      idx += 1;
    write_volatile(table.offset(idx), res.phentsize);  idx += 1;
    write_volatile(table.offset(idx), AT_PHNUM);      idx += 1;
    write_volatile(table.offset(idx), res.phnum);     idx += 1;
    write_volatile(table.offset(idx), AT_PAGESZ);     idx += 1;
    write_volatile(table.offset(idx), 4096u64);       idx += 1;
    if let Some((at_base, at_entry)) = res.dynamic_linker {
        write_volatile(table.offset(idx), AT_BASE);   idx += 1;
        write_volatile(table.offset(idx), at_base);   idx += 1;
        write_volatile(table.offset(idx), AT_ENTRY);  idx += 1;
        write_volatile(table.offset(idx), at_entry);  idx += 1;
        write_volatile(table.offset(idx), AT_FLAGS);  idx += 1;
        write_volatile(table.offset(idx), 0u64);      idx += 1;
    } else {
        write_volatile(table.offset(idx), AT_BASE);   idx += 1;
        write_volatile(table.offset(idx), 0u64);      idx += 1;
        write_volatile(table.offset(idx), AT_ENTRY);  idx += 1;
        write_volatile(table.offset(idx), res.entry_point); idx += 1;
    }
    write_volatile(table.offset(idx), AT_RANDOM);     idx += 1;
    write_volatile(table.offset(idx), random_ptr);    idx += 1;
    write_volatile(table.offset(idx), AT_NULL);       idx += 1;
    write_volatile(table.offset(idx), 0u64);

    let entry_point = res.entry_point;

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
        "mov ecx, 0xC0000100",
        "mov eax, r11d",
        "mov rdx, r11",
        "shr rdx, 32",
        "wrmsr",
        "mov ax, 0x23",
        "mov gs, ax",
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
        usp = in(reg) rsp,
        ss  = in(reg) 0x23u64,
        in("r11") tls_base,
        options(noreturn)
    );
}
