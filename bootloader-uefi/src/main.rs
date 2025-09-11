#![no_std]
#![no_main]

use core::fmt::Write;
use core::slice;
use uefi::prelude::*;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, Directory, RegularFile, FileAttribute, FileInfo, FileMode};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::{AllocateType, MemoryType, BootServices, OpenProtocolParams, OpenProtocolAttributes, SearchType};
use uefi::proto::console::gop::GraphicsOutput;
use uefi::Identify;

// Estructura para pasar información del framebuffer al kernel
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub width: u32,
    pub height: u32,
    pub pixels_per_scan_line: u32,
    pub pixel_format: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub reserved_mask: u32,
}

// Global allocator simple
struct SimpleAllocator;

unsafe impl core::alloc::GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, _layout: core::alloc::Layout) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {
        // No-op
    }
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator;

// Panic handler
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe {
            // TEMPORALMENTE DESHABILITADO: hlt causa opcode inválido
            // Simular halt con spin loop
            for _ in 0..10000 {
                core::hint::spin_loop();
            }
        }
    }
}

const KERNEL_PHYS_LOAD_ADDR: u64 = 0x0020_0000;
const PT_LOAD: u32 = 1;

#[inline(always)]
fn pages_for_size(size: usize) -> usize { (size + 0xFFF) / 0x1000 }

fn open_root_fs(bs: &BootServices, image_handle: Handle) -> uefi::Result<Directory> {
    let image = bs.open_protocol_exclusive::<LoadedImage>(image_handle)?;
    let device_handle = image.device().expect("LoadedImage without device handle");
    let mut fs = bs.open_protocol_exclusive::<SimpleFileSystem>(device_handle)?;
    fs.open_volume()
}

fn open_kernel_file(root: &mut Directory) -> uefi::Result<RegularFile> {
    // Ampliar rutas candidatas para localizar el kernel en la ESP
    let candidates = [
        // Nombres base
        uefi::cstr16!("eclipse_kernel"),
        uefi::cstr16!("\\eclipse_kernel"),
    ];
    for p in candidates.iter() {
        if let Ok(file) = root.open(p, FileMode::Read, FileAttribute::empty()) {
            if let Some(reg) = file.into_regular_file() {
                return Ok(reg);
            }
        }
    }
    Err(uefi::Status::NOT_FOUND.into())
}

fn read_file_size(file: &mut RegularFile) -> Result<usize, Status> {
    let mut info_buf = [0u8; 1024];
    match file.get_info::<FileInfo>(&mut info_buf) {
        Ok(info) => Ok(info.file_size() as usize),
        Err(e) => Err(e.status()),
    }
}

// Salida serie COM1 para diagnóstico temprano
#[inline(always)]
unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

#[inline(always)]
unsafe fn inb(port: u16) -> u8 {
    let mut val: u8;
    core::arch::asm!("in al, dx", in("dx") port, out("al") val, options(nomem, nostack, preserves_flags));
    val
}

unsafe fn serial_init() {
    let base: u16 = 0x3F8;
    outb(base + 1, 0x00);
    outb(base + 3, 0x80);
    outb(base + 0, 0x01);
    outb(base + 1, 0x00);
    outb(base + 3, 0x03);
    outb(base + 2, 0xC7);
    outb(base + 4, 0x0B);
}

unsafe fn serial_write_byte(b: u8) {
    let base: u16 = 0x3F8;
    while (inb(base + 5) & 0x20) == 0 {}
    outb(base, b);
}

unsafe fn serial_write_str(s: &str) {
    for &c in s.as_bytes() { serial_write_byte(c); }
}

unsafe fn serial_write_hex32(val: u32) {
    for i in (0..8).rev() {
        let nibble = (val >> (i * 4)) & 0xF;
        let c = if nibble < 10 {
            b'0' + nibble as u8
        } else {
            b'A' + (nibble - 10) as u8
        };
        serial_write_byte(c);
    }
}

unsafe fn serial_write_hex64(val: u64) {
    for i in (0..16).rev() {
        let nibble = (val >> (i * 4)) & 0xF;
        let c = if nibble < 10 {
            b'0' + nibble as u8
        } else {
            b'A' + (nibble - 10) as u8
        };
        serial_write_byte(c);
    }
}

unsafe fn serial_write_hex8(val: u8) {
    let high = (val >> 4) & 0xF;
    let low = val & 0xF;
    let c1 = if high < 10 {
        b'0' + high
    } else {
        b'A' + (high - 10)
    };
    let c2 = if low < 10 {
        b'0' + low
    } else {
        b'A' + (low - 10)
    };
    serial_write_byte(c1);
    serial_write_byte(c2);
}

// ELF64 estructuras mínimas
#[repr(C)]
struct Elf64Ehdr {
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

#[repr(C)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

const MAX_PH: usize = 64;

struct SegmentMap {
    count: usize,
    vstart: [u64; MAX_PH],
    pstart: [u64; MAX_PH],
    len: [u64; MAX_PH],
}

fn load_elf64_segments(bs: &BootServices, file: &mut RegularFile) -> Result<(u64, u64, u64, SegmentMap), BootError> {
    // Posicionar al inicio y leer cabecera ELF de forma exacta
    let _ = file.set_position(0);
    let mut ehdr_buf = [0u8; core::mem::size_of::<Elf64Ehdr>()];
    {
        let mut total = 0usize;
        while total < ehdr_buf.len() {
            match file.read(&mut ehdr_buf[total..]) {
                Ok(n) if n > 0 => total += n,
                Ok(_) => return Err(BootError::LoadElf(Status::LOAD_ERROR)),
                Err(e) => return Err(BootError::LoadElf(e.status())),
            }
        }
    }
    let ehdr: Elf64Ehdr = unsafe { core::ptr::read_unaligned(ehdr_buf.as_ptr() as *const Elf64Ehdr) };

    // Debug: verificar entry point
    unsafe {
        serial_write_str("DEBUG: raw e_entry bytes: ");
        for i in 0..8 {
            let byte = ehdr_buf[24 + i];
            serial_write_hex32(byte as u32);
            serial_write_str(" ");
        }
        serial_write_str("\r\n");
        serial_write_str("DEBUG: ehdr.e_entry = 0x");
        serial_write_hex64(ehdr.e_entry);
        serial_write_str("\r\n");
    }

    // Validaciones básicas
    if &ehdr.e_ident[0..4] != b"\x7FELF" { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }
    if ehdr.e_ident[4] != 2 { return Err(BootError::LoadElf(Status::UNSUPPORTED)); } // 64-bit
    if ehdr.e_machine != 62 { return Err(BootError::LoadElf(Status::UNSUPPORTED)); } // x86_64

    // Iterar program headers
    let phentsize = ehdr.e_phentsize as usize;
    let phnum = ehdr.e_phnum as usize;
    let mut ph_buf = [0u8; core::mem::size_of::<Elf64Phdr>()];

    // Asegurar tamaño de entrada de programa esperado para ELF64
    if phentsize != core::mem::size_of::<Elf64Phdr>() {
        return Err(BootError::LoadElf(Status::UNSUPPORTED));
    }

    // Reservar memoria física en dirección fija para el kernel
    let kernel_phys_base = KERNEL_PHYS_LOAD_ADDR;

    // PASO 1: calcular el rango virtual del kernel y recolectar información de segmentos
    let mut min_vaddr: u64 = u64::MAX;
    let mut max_vaddr: u64 = 0;
    let mut segmap = SegmentMap { count: 0, vstart: [0; MAX_PH], pstart: [0; MAX_PH], len: [0; MAX_PH] };

    // Primer pase: calcular direcciones virtuales y tamaño total
    for i in 0..phnum {
        let off = ehdr.e_phoff + (i as u64) * (ehdr.e_phentsize as u64);
        if file.set_position(off).is_err() { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }
        if phentsize > ph_buf.len() { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }
        ph_buf.fill(0);
        {
            let mut got = 0usize;
            while got < phentsize {
                match file.read(&mut ph_buf[got..phentsize]) {
                    Ok(n) if n > 0 => got += n,
                    Ok(_) => return Err(BootError::LoadElf(Status::LOAD_ERROR)),
                    Err(e) => return Err(BootError::LoadElf(e.status())),
                }
            }
        }
        let phdr: Elf64Phdr = unsafe { core::ptr::read_unaligned(ph_buf.as_ptr() as *const Elf64Phdr) };
        if phdr.p_type != PT_LOAD { continue; }

        // Calcular direcciones virtuales del segmento
        let vaddr_start = phdr.p_vaddr & !0xFFFu64;
        let vaddr_end = (phdr.p_vaddr + phdr.p_memsz + 0xFFF) & !0xFFFu64;

        if vaddr_start < min_vaddr { min_vaddr = vaddr_start; }
        if vaddr_end > max_vaddr { max_vaddr = vaddr_end; }
    }

    if min_vaddr == u64::MAX || max_vaddr <= min_vaddr { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }

    // Calcular tamaño total del kernel
    let total_size = max_vaddr - min_vaddr;
    let total_pages = ((total_size + 0xFFF) / 0x1000) as usize;

    // Reservar memoria física - usar BOOT_SERVICES_CODE para que UEFI no la reutilice después de exit
    if let Err(st) = bs.allocate_pages(AllocateType::Address(kernel_phys_base), MemoryType::BOOT_SERVICES_CODE, total_pages) {
        return Err(BootError::LoadSegment { status: st.status(), seg_index: 0, addr: kernel_phys_base, pages: total_pages });
    }

    // Segundo pase: llenar el mapa de segmentos con direcciones físicas correctas
    let mut seg_idx = 0;
    for i in 0..phnum {
        let off = ehdr.e_phoff + (i as u64) * (ehdr.e_phentsize as u64);
        if file.set_position(off).is_err() { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }
        if phentsize > ph_buf.len() { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }
        ph_buf.fill(0);
        {
            let mut got = 0usize;
            while got < phentsize {
                match file.read(&mut ph_buf[got..phentsize]) {
                    Ok(n) if n > 0 => got += n,
                    Ok(_) => return Err(BootError::LoadElf(Status::LOAD_ERROR)),
                    Err(e) => return Err(BootError::LoadElf(e.status())),
                }
            }
        }
        let phdr: Elf64Phdr = unsafe { core::ptr::read_unaligned(ph_buf.as_ptr() as *const Elf64Phdr) };
        if phdr.p_type != PT_LOAD { continue; }

        // Calcular direcciones virtuales del segmento
        let vaddr_start = phdr.p_vaddr & !0xFFFu64;
        let vaddr_end = (phdr.p_vaddr + phdr.p_memsz + 0xFFF) & !0xFFFu64;

        if seg_idx < MAX_PH {
            segmap.vstart[seg_idx] = vaddr_start;
            segmap.pstart[seg_idx] = kernel_phys_base + (vaddr_start - min_vaddr); // Dirección física correspondiente
            segmap.len[seg_idx] = vaddr_end.saturating_sub(vaddr_start);
            seg_idx += 1;
        }
    }
    segmap.count = seg_idx;

    // PASO 2: copiar datos y zerofill por segmento (sin más reservas)
    for i in 0..phnum {
        let off = ehdr.e_phoff + (i as u64) * (ehdr.e_phentsize as u64);
        if file.set_position(off).is_err() { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }
        if phentsize > ph_buf.len() { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }
        ph_buf.fill(0);
        {
            let mut got = 0usize;
            while got < phentsize {
                match file.read(&mut ph_buf[got..phentsize]) {
                    Ok(n) if n > 0 => got += n,
                    Ok(_) => return Err(BootError::LoadElf(Status::LOAD_ERROR)),
                    Err(e) => return Err(BootError::LoadElf(e.status())),
                }
            }
        }
        let phdr: Elf64Phdr = unsafe { core::ptr::read_unaligned(ph_buf.as_ptr() as *const Elf64Phdr) };
        if phdr.p_type != PT_LOAD { continue; }

        // Calcular offset del segmento dentro del kernel cargado
        let segment_offset = phdr.p_vaddr - min_vaddr;
        let dest_phys = kernel_phys_base + segment_offset;

        // Debug: mostrar información de carga del segmento
        unsafe {
            serial_write_str("DEBUG: segmento p_vaddr=0x");
            serial_write_hex64(phdr.p_vaddr);
            serial_write_str(" p_offset=0x");
            serial_write_hex64(phdr.p_offset);
            serial_write_str(" dest_phys=0x");
            serial_write_hex64(dest_phys);
            serial_write_str(" p_filesz=0x");
            serial_write_hex64(phdr.p_filesz);
            serial_write_str("\r\n");
        }

        // Leer datos del segmento
        if file.set_position(phdr.p_offset).is_err() { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }
        if phdr.p_filesz > 0 {
            let dst_ptr = dest_phys as *mut u8;
            let mut remaining = phdr.p_filesz as usize;
            let mut offset = 0usize;
            while remaining > 0 {
                let chunk = unsafe { slice::from_raw_parts_mut(dst_ptr.add(offset), remaining) };
                match file.read(chunk) {
                    Ok(n) if n > 0 => { remaining -= n; offset += n; },
                    Ok(_) => return Err(BootError::LoadElf(Status::LOAD_ERROR)),
                    Err(e) => return Err(BootError::LoadElf(e.status())),
                }
            }

            // Debug: verificar que se cargó correctamente
            unsafe {
                serial_write_str("DEBUG: verificando carga en 0x");
                serial_write_hex64(dest_phys);
                serial_write_str(": ");
                for i in 0..16 {
                    let b = core::ptr::read_volatile(dst_ptr.add(i));
                    serial_write_hex32(b as u32);
                    serial_write_str(" ");
                }
                serial_write_str("\r\n");
            }
        }
        // Cero para .bss
        if phdr.p_memsz > phdr.p_filesz {
            let zero_ptr = (dest_phys + phdr.p_filesz) as *mut u8;
            let zero_len = (phdr.p_memsz - phdr.p_filesz) as usize;
            unsafe { core::ptr::write_bytes(zero_ptr, 0, zero_len); }
        }
    }

    // Debug: mostrar valores calculados
    unsafe {
        serial_write_str("DEBUG: min_vaddr=0x");
        serial_write_hex64(min_vaddr);
        serial_write_str(" max_vaddr=0x");
        serial_write_hex64(max_vaddr);
        serial_write_str(" kernel_phys_base=0x");
        serial_write_hex64(kernel_phys_base);
        serial_write_str("\r\n");
    }

    // Ajustar el entry point al offset físico donde se cargó el kernel
    let entry = if ehdr.e_entry != 0 {
        ehdr.e_entry  // Mantener la dirección virtual del entry point
    } else {
        min_vaddr  // Si no hay entry point específico, usar el inicio del kernel
    };

    let total_len = max_vaddr - min_vaddr;
    Ok((entry, kernel_phys_base, total_len, segmap))
}

fn load_kernel_from_data(bs: &BootServices, kernel_data: &[u8]) -> Result<(u64, u64, u64), BootError> {
    // Leer cabecera ELF
    if kernel_data.len() < core::mem::size_of::<Elf64Ehdr>() {
        return Err(BootError::LoadElf(Status::LOAD_ERROR));
    }

    let ehdr: Elf64Ehdr = unsafe { core::ptr::read_unaligned(kernel_data.as_ptr() as *const Elf64Ehdr) };

    // Validaciones básicas
    if &ehdr.e_ident[0..4] != b"\x7FELF" { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }
    if ehdr.e_ident[4] != 2 { return Err(BootError::LoadElf(Status::UNSUPPORTED)); } // 64-bit
    if ehdr.e_machine != 62 { return Err(BootError::LoadElf(Status::UNSUPPORTED)); } // x86_64

    // Iterar program headers
    let phentsize = ehdr.e_phentsize as usize;
    let phnum = ehdr.e_phnum as usize;

    // Asegurar tamaño de entrada de programa esperado para ELF64
    if phentsize != core::mem::size_of::<Elf64Phdr>() {
        return Err(BootError::LoadElf(Status::UNSUPPORTED));
    }

    // Reservar memoria física en dirección fija para el kernel
    let kernel_phys_base = KERNEL_PHYS_LOAD_ADDR;

    // Calcular tamaño total necesario
    let mut total_size = 0u64;
    for i in 0..phnum {
        let off = ehdr.e_phoff + (i as u64) * (ehdr.e_phentsize as u64);
        if off as usize + phentsize > kernel_data.len() {
            return Err(BootError::LoadElf(Status::LOAD_ERROR));
        }

        let phdr: Elf64Phdr = unsafe {
            core::ptr::read_unaligned(kernel_data[off as usize..].as_ptr() as *const Elf64Phdr)
        };

        if phdr.p_type != PT_LOAD { continue; }

        let segment_end = phdr.p_vaddr + phdr.p_memsz;
        if segment_end > total_size {
            total_size = segment_end;
        }
    }

    let total_pages = ((total_size + 0xFFF) / 0x1000) as usize;

    // Reservar memoria física
    if let Err(st) = bs.allocate_pages(AllocateType::Address(kernel_phys_base), MemoryType::BOOT_SERVICES_CODE, total_pages) {
        return Err(BootError::LoadSegment { status: st.status(), seg_index: 0, addr: kernel_phys_base, pages: total_pages });
    }

    // Cargar segmentos
    for i in 0..phnum {
        let off = ehdr.e_phoff + (i as u64) * (ehdr.e_phentsize as u64);
        let phdr: Elf64Phdr = unsafe {
            core::ptr::read_unaligned(kernel_data[off as usize..].as_ptr() as *const Elf64Phdr)
        };

        if phdr.p_type != PT_LOAD { continue; }

        // Calcular dirección física donde cargar el segmento
        let dest_phys = kernel_phys_base + phdr.p_vaddr;

        // Cargar datos del segmento
        if phdr.p_filesz > 0 {
            if phdr.p_offset as usize + phdr.p_filesz as usize > kernel_data.len() {
                return Err(BootError::LoadElf(Status::LOAD_ERROR));
            }

            let src_data = &kernel_data[phdr.p_offset as usize..phdr.p_offset as usize + phdr.p_filesz as usize];
            let dst_ptr = dest_phys as *mut u8;

            unsafe {
                core::ptr::copy_nonoverlapping(src_data.as_ptr(), dst_ptr, phdr.p_filesz as usize);
            }
        }

        // Inicializar memoria .bss (ceros)
        if phdr.p_memsz > phdr.p_filesz {
            let bss_ptr = (dest_phys + phdr.p_filesz) as *mut u8;
            let bss_len = (phdr.p_memsz - phdr.p_filesz) as usize;
            unsafe { core::ptr::write_bytes(bss_ptr, 0, bss_len); }
        }
    }

    let entry_point = if ehdr.e_entry != 0 {
        ehdr.e_entry
    } else {
        kernel_phys_base // fallback
    };

    Ok((entry_point, kernel_phys_base, total_size))
}

enum BootError {
    OpenRoot(Status),
    OpenKernel(Status),
    LoadElf(Status),
    LoadSegment { status: Status, seg_index: usize, addr: u64, pages: usize },
    AllocStack(Status),
    AllocPml4(Status),
    AllocPdpt(Status),
    AllocPd(Status),
}

fn prepare_page_tables_only(bs: &BootServices, handle: Handle) -> core::result::Result<(u64, u64), BootError> {
    // Debug: confirmar que la función se está ejecutando
    unsafe { serial_write_str("DEBUG: prepare_page_tables_only started\r\n"); }

    // Reservar stack (64 KiB) por debajo de 1 GiB para que esté mapeada (identidad 0..1GiB)
    let stack_pages: usize = 16;
    let one_gib: u64 = 1u64 << 30;
    let max_stack_addr: u64 = one_gib - 0x1000; // límite superior < 1GiB
    let stack_base = bs
        .allocate_pages(AllocateType::MaxAddress(max_stack_addr), MemoryType::BOOT_SERVICES_DATA, stack_pages)
        .map_err(|e| BootError::AllocStack(e.status()))?;
    let stack_top = stack_base + (stack_pages as u64) * 4096u64;

    // Configurar paginación identidad extendida (16 GiB, páginas de 2 MiB) para cubrir direcciones altas del kernel
    // Allocate: 1 página para PML4, 2 para PDPT (para cubrir 512 GiB cada uno), 16 para PD (1 por GiB)
    let pml4_phys = bs
        .allocate_pages(AllocateType::AnyPages, MemoryType::BOOT_SERVICES_DATA, 1)
        .map_err(|e| BootError::AllocPml4(e.status()))?;

    // Necesitamos 2 PDPT para cubrir direcciones hasta ~1 TiB
    let pdpt_phys = bs
        .allocate_pages(AllocateType::AnyPages, MemoryType::BOOT_SERVICES_DATA, 1)
        .map_err(|e| BootError::AllocPdpt(e.status()))?;
    let pdpt_phys_high = bs
        .allocate_pages(AllocateType::AnyPages, MemoryType::BOOT_SERVICES_DATA, 1)
        .map_err(|e| BootError::AllocPdpt(e.status()))?;

    // 16 PD para cubrir 16 GiB
    let mut pd_phys_arr: [u64; 16] = [0; 16];
    for gi_b in 0..16 {
        pd_phys_arr[gi_b] = bs
            .allocate_pages(AllocateType::AnyPages, MemoryType::BOOT_SERVICES_DATA, 1)
            .map_err(|e| BootError::AllocPd(e.status()))?;
    }

        unsafe {
        // Limpiar tablas
        core::ptr::write_bytes(pml4_phys as *mut u8, 0, 4096);
        core::ptr::write_bytes(pdpt_phys as *mut u8, 0, 4096);
        core::ptr::write_bytes(pdpt_phys_high as *mut u8, 0, 4096);
        for gi_b in 0..16 { core::ptr::write_bytes(pd_phys_arr[gi_b] as *mut u8, 0, 4096); }

        // Flags
        let p_w = 0x003u64; // Present | Write
        let ps = 0x080u64; // Page Size (2 MiB) en PDE

        // PML4[0] -> PDPT
        let pml4 = pml4_phys as *mut u64;
        *pml4.add(0) = (pdpt_phys & 0x000F_FFFF_FFFF_F000u64) | p_w;

        // PDPT[0..7] -> PDs (cada entrada mapea 1 GiB)
        let pdpt = pdpt_phys as *mut u64;
        for gi_b in 0..8u64 {
            *pdpt.add(gi_b as usize) = (pd_phys_arr[gi_b as usize] & 0x000F_FFFF_FFFF_F000u64) | p_w;
            let pd = pd_phys_arr[gi_b as usize] as *mut u64;
            for i in 0..512u64 {
                // Identidad por defecto
                let mut phys_base = gi_b * 0x4000_0000u64 + i * 0x20_0000u64;
                // Alias fijo: VA [4GiB,5GiB) -> PA [0,1GiB)
                if gi_b == 4 { phys_base = i * 0x20_0000u64; }
                *pd.add(i as usize) = (phys_base & 0x000F_FFFF_FFFF_F000u64) | p_w | ps;
            }
        }

        // PDPT[8..15] -> PDs usando pdpt_phys_high (para direcciones altas)
        let pdpt_high = pdpt_phys_high as *mut u64;
        for gi_b in 8..16u64 {
            let pd_idx = gi_b - 8; // 0..7 para el segundo PDPT
            *pdpt_high.add(pd_idx as usize) = (pd_phys_arr[gi_b as usize] & 0x000F_FFFF_FFFF_F000u64) | p_w;
            let pd = pd_phys_arr[gi_b as usize] as *mut u64;
            for i in 0..512u64 {
                // Identidad para direcciones altas
                let phys_base = gi_b * 0x4000_0000u64 + i * 0x20_0000u64;
                *pd.add(i as usize) = (phys_base & 0x000F_FFFF_FFFF_F000u64) | p_w | ps;
            }
        }

        // Conectar el segundo PDPT al PML4 en la entrada 1 (para direcciones >= 512 GiB)
        *pml4.add(1) = (pdpt_phys_high & 0x000F_FFFF_FFFF_F000u64) | p_w;

        // Mapear framebuffer si es necesario (dirección típica 0x80000000+)
        // Nota: framebuffer_info no está disponible aquí, se maneja después

        // Mapear kernel específicamente: VA 0x200000 -> PA 0x0020_0000
        {
            let kernel_va = 0x200000u64;
            let kernel_pa = 0x0020_0000u64;

            // Calcular índices sin alinear
            let pdpt_idx = (kernel_va >> 30) & 0x1FF; // 0
            let pd_idx = (kernel_va >> 21) & 0x1FF;   // 1 (0x200000 / 0x200000 = 1)

            // Alinear PA a 2MB
            let kernel_pa_aligned = kernel_pa & !0x1F_FFFFu64;

            if pdpt_idx < 8 {
                let pd = pd_phys_arr[pdpt_idx as usize] as *mut u64;
                // Mapear PD[1] que controla VA 0x0020_0000 - 0x003F_FFFF
                *pd.add(pd_idx as usize) = (kernel_pa_aligned & 0x000F_FFFF_FFFF_F000u64) | p_w | ps;
            }
        }

        // Nota: mantenemos mapeo identidad 0–8 GiB
    }

    unsafe { serial_write_str("DEBUG: prepare_page_tables_only completed\r\n"); }
    Ok((pml4_phys, stack_top))
}

#[entry]
fn main(handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // Serial init temprano
    unsafe {
        serial_write_str("PRE-SERIAL-INIT\r\n");
        serial_init();
        serial_write_str("BL: inicio\r\n");
        serial_write_str("DEBUG: serial initialized\r\n");
    }

    // Mensaje de debug en consola UEFI también
    {
        let out = system_table.stdout();
        let _ = out.write_str("UEFI DEBUG: Bootloader started, serial init complete\r\n");
    }
    // Mensaje inicial
    {
        let out = system_table.stdout();
        let _ = out.write_str("Eclipse OS Bootloader UEFI\n");
        let _ = out.write_str("Cargando kernel ELF...\n");
    }

    // Preparación con reporte de error detallado
    unsafe { serial_write_str("DEBUG: about to call prepare_boot_environment\r\n"); }
    let out = system_table.stdout();
    let _ = out.write_str("DEBUG CONSOLE: about to call prepare_boot_environment\r\n");
    // Preparar solo las tablas de páginas ANTES de exit_boot_services
    let (pml4_phys, stack_top): (u64, u64) = {
        let bs = system_table.boot_services();
        unsafe { serial_write_str("DEBUG: got boot services\r\n"); }
        match prepare_page_tables_only(bs, handle) {
            Ok((pml4, stack)) => {
                unsafe { serial_write_str("DEBUG: prepare_page_tables_only returned OK\r\n"); }
                (pml4, stack)
            },
            Err(err) => {
                let mut out = system_table.stdout();
                let _ = out.write_str("ERROR preparando tablas de páginas: ");
                match err {
                    BootError::AllocStack(st) => { let _ = out.write_str("allocate_pages stack "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                    BootError::AllocPml4(st) => { let _ = out.write_str("allocate_pages PML4 "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                    BootError::AllocPdpt(st) => { let _ = out.write_str("allocate_pages PDPT "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                    BootError::AllocPd(st) => { let _ = out.write_str("allocate_pages PD "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                    _ => { let _ = out.write_str("otro error"); }
                }
                let _ = out.write_str("\n");
                // Valores por defecto en caso de error
                (0, 0)
            }
        }
    };

    // Obtener información del framebuffer ANTES de salir de Boot Services
    let mut framebuffer_info = FramebufferInfo {
        base_address: 0,
        width: 0,
        height: 0,
        pixels_per_scan_line: 0,
        pixel_format: 0,
        red_mask: 0,
        green_mask: 0,
        blue_mask: 0,
        reserved_mask: 0,
    };
    let mut framebuffer_info_ptr: u64 = 0;
    
    // Intentar obtener información del framebuffer usando Graphics Output Protocol
    {
        let bs = system_table.boot_services();
        // Buscar el protocolo GOP en todos los handles disponibles
        let mut gop_protocol = None;
        
        // Obtener todos los handles
        if let Ok(handles) = bs.locate_handle_buffer(SearchType::ByProtocol(&GraphicsOutput::GUID)) {
            for gop_handle in handles.iter() {
                if let Ok(gop) = unsafe { 
                    bs.open_protocol::<GraphicsOutput>(
                        OpenProtocolParams {
                            handle: *gop_handle,
                            agent: handle,
                            controller: None,
                        },
                        OpenProtocolAttributes::GetProtocol,
                    )
                } {
                    gop_protocol = Some(gop);
                    break;
                }
            }
        }
        
        if let Some(mut gop) = gop_protocol {
            let mode = gop.current_mode_info();
            // Obtener información del framebuffer desde el protocolo GOP
            let mut frame_buffer = gop.frame_buffer();
            framebuffer_info.base_address = frame_buffer.as_mut_ptr() as u64;
            framebuffer_info.width = mode.resolution().0 as u32;
            framebuffer_info.height = mode.resolution().1 as u32;
            framebuffer_info.pixels_per_scan_line = mode.stride() as u32;
            framebuffer_info.pixel_format = mode.pixel_format() as u32;
            if let Some(mask) = mode.pixel_bitmask() {
                framebuffer_info.red_mask = mask.red;
                framebuffer_info.green_mask = mask.green;
                framebuffer_info.blue_mask = mask.blue;
                framebuffer_info.reserved_mask = mask.reserved;
            }
            // Reservar memoria persistente para pasar al kernel
            if let Ok(phys) = bs.allocate_pages(AllocateType::AnyPages, MemoryType::BOOT_SERVICES_DATA, 1) {
                framebuffer_info_ptr = phys;
                unsafe {
                    let dst = phys as *mut FramebufferInfo;
                    core::ptr::write_volatile(dst, framebuffer_info);
                }
            }
            
            unsafe { 
                serial_write_str("BL: GOP encontrado\r\n");
                // Log puntero de framebuffer_info reservado (si existe)
                serial_write_str("BL: fbptr_pre=0x");
                let mut h = [0u8; 18];
                let mut m = 0usize;
                for i in (0..16).rev() {
                    let nyb = ((framebuffer_info_ptr >> (i*4)) & 0xF) as u8;
                    h[m] = if nyb < 10 { b'0'+nyb } else { b'a'+(nyb-10) }; m+=1;
                }
                h[m] = b'\r'; m+=1; h[m] = b'\n'; m+=1;
                for i in 0..m { serial_write_byte(h[i]); }
                // Log de información del framebuffer
                let mut buf = [0u8; 32];
                let mut n = 0usize;
                
                // Log base_address
                serial_write_str("BL: base=0x");
                for i in (0..16).rev() {
                    let nyb = ((framebuffer_info.base_address >> (i*4)) & 0xF) as u8;
                    buf[n] = if nyb < 10 { b'0'+nyb } else { b'a'+(nyb-10) }; n+=1;
                }
                buf[n] = b'\r'; n+=1; buf[n] = b'\n'; n+=1;
                for i in 0..n { serial_write_byte(buf[i]); }
                
                // Log resolución
                serial_write_str("BL: res="); 
                // Width
                n = 0;
                let w = framebuffer_info.width;
                if w == 0 { 
                    buf[n] = b'0'; n+=1; 
                } else {
                    let mut temp = w;
                    let mut digits = [0u8; 8];
                    let mut digit_count = 0;
                    while temp > 0 {
                        digits[digit_count] = b'0' + (temp % 10) as u8;
                        temp /= 10;
                        digit_count += 1;
                    }
                    // Escribir dígitos en orden correcto (invertir)
                    for i in (0..digit_count).rev() {
                        buf[n] = digits[i];
                        n += 1;
                    }
                }
                buf[n] = b'x'; n+=1;
                // Height
                let h = framebuffer_info.height;
                if h == 0 { 
                    buf[n] = b'0'; n+=1; 
                } else {
                    let mut temp = h;
                    let mut digits = [0u8; 8];
                    let mut digit_count = 0;
                    while temp > 0 {
                        digits[digit_count] = b'0' + (temp % 10) as u8;
                        temp /= 10;
                        digit_count += 1;
                    }
                    // Escribir dígitos en orden correcto (invertir)
                    for i in (0..digit_count).rev() {
                        buf[n] = digits[i];
                        n += 1;
                    }
                }
                buf[n] = b'\r'; n+=1; buf[n] = b'\n'; n+=1;
                for i in 0..n { serial_write_byte(buf[i]); }
            }
        } else {
            unsafe { 
                serial_write_str("BL: GOP no encontrado, usando VGA\r\n");
            }
        }
    }

    // Logs de depuración ANTES de salir de Boot Services
    {
        use core::fmt::Write as _;
        let out = system_table.stdout();
        let _ = out.write_str("Preparando para cargar kernel después de ExitBootServices...\n");
        let _ = out.write_str("\n");
        let _ = out.write_str("Saliendo de Boot Services...\n");
        unsafe { serial_write_str("BL: antes ExitBootServices\r\n"); }
    }

    // Preparar el entry point del kernel
    let mut entry_reg = 0x200000u64; // Entry point virtual del kernel (valor por defecto)

    // CARGAR EL KERNEL ANTES DE EXIT_BOOT_SERVICES
    let (kernel_entry, kernel_base, kernel_size) = {
        let bs = system_table.boot_services();
        let kernel_data = include_bytes!("../../eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel");

        match load_kernel_from_data(bs, kernel_data) {
            Ok((entry_point, kernel_phys_base, total_len)) => {
                unsafe {
                    serial_write_str("BL: kernel ELF cargado exitosamente\r\n");
                    serial_write_str("BL: entry_point=0x");
                    serial_write_hex64(entry_point);
                    serial_write_str(" kernel_phys_base=0x");
                    serial_write_hex64(kernel_phys_base);
                    serial_write_str(" total_len=0x");
                    serial_write_hex64(total_len);
                    serial_write_str("\r\n");
                }
                entry_reg = entry_point;
                (entry_point, kernel_phys_base, total_len)
            },
            Err(e) => {
                unsafe {
                    serial_write_str("BL: ERROR cargando kernel ELF: ");
                    match e {
                        BootError::LoadElf(st) => {
                            serial_write_str("LoadElf status=");
                            serial_write_hex64(st.0 as u64);
                        },
                        BootError::LoadSegment { status, seg_index, addr, pages } => {
                            serial_write_str("LoadSegment seg=");
                            serial_write_hex64(seg_index as u64);
                            serial_write_str(" addr=0x");
                            serial_write_hex64(addr);
                            serial_write_str(" pages=");
                            serial_write_hex64(pages as u64);
                            serial_write_str(" status=");
                            serial_write_hex64(status.0 as u64);
                        },
                        _ => serial_write_str("Otro error"),
                    }
                    serial_write_str("\r\n");
                }
                // Mantener valores por defecto si falla la carga
                (0x200000u64, KERNEL_PHYS_LOAD_ADDR, 0)
            }
        }
    };

    // ExitBootServices (uefi 0.25.0)
    // IMPORTANTE: Usar BOOT_SERVICES_CODE para mantener la memoria del kernel intacta
    let (_rt_st, _final_map) = system_table.exit_boot_services(MemoryType::BOOT_SERVICES_CODE);
    unsafe { serial_write_str("BL: despues ExitBootServices\r\n"); }
    // Nota: Framebuffer mapping simplificado por ahora

    // Configurar paginación identidad y pila y saltar al kernel (sin usar la pila después de cambiarla)
    unsafe {

        // Debug: mostrar valores antes de configurar CR3
        serial_write_str("BL: entry_reg=0x");
        {
            let mut buf = [0u8; 16];
            let mut n = 0usize;
            for i in (0..16).rev() {
                let nyb = ((entry_reg >> (i*4)) & 0xF) as u8;
                buf[n] = if nyb < 10 { b'0'+nyb } else { b'a'+(nyb-10) }; n+=1;
            }
            for i in 0..n { serial_write_byte(buf[i]); }
        }
        serial_write_str(" pml4_phys=0x");
        {
            let mut buf = [0u8; 16];
            let mut n = 0usize;
            for i in (0..16).rev() {
                let nyb = ((pml4_phys >> (i*4)) & 0xF) as u8;
                buf[n] = if nyb < 10 { b'0'+nyb } else { b'a'+(nyb-10) }; n+=1;
            }
            for i in 0..n { serial_write_byte(buf[i]); }
        }
        serial_write_str("\r\n");

        // DEBUG: Verificar estado antes del salto
        serial_write_str("BL: DEBUG antes del salto:\r\n");
        serial_write_str("BL: entry_reg=");
        serial_write_hex64(entry_reg);
        serial_write_str(" pml4_phys=");
        serial_write_hex64(pml4_phys);
        serial_write_str(" stack_top=");
        serial_write_hex64(stack_top);
        serial_write_str("\r\n");

        // Verificar que el código esté en la dirección física correcta
        serial_write_str("BL: verificando kernel en PA 0x00200000: ");
        let kernel_ptr = 0x00200000 as *const u8;
        for i in 0..16 {
            let byte = unsafe { *kernel_ptr.add(i) };
            serial_write_hex8(byte);
            serial_write_str(" ");
        }
        serial_write_str("\r\n");

        // DEBUG: Mostrar qué va a pasar justo antes del assembly
        serial_write_str("BL: saltando a kernel entry point...\r\n");

        // CONFIGURAR PAGINACIÓN CON MAPEO DE IDENTIDAD SIMPLE
        serial_write_str("BL: configurando paginacion con identidad...\r\n");

        // Configurar CR3 con el PML4 que ya tenemos (que tiene mapeo de identidad)
        let cr3_value = pml4_phys;
        let rsp_alineado = stack_top & !0xFu64;

        // SALTAR AL KERNEL PASANDO INFORMACIÓN DEL FRAMEBUFFER
        // ⚠️  INSTRUCCIONES CRÍTICAS PARA BOOTLOADER - NO DESHABILITAR ⚠️
        // Estas instrucciones SON necesarias para que el bootloader funcione

        // Configurar SSE (necesario para evitar #UD)
        core::arch::asm!(
            "mov rax, cr0",
            "and rax, ~(1 << 2)",        // CR0.EM = 0
            "or  rax,  (1 << 1)",        // CR0.MP = 1
            "mov cr0, rax",
            "mov rax, cr4",
            "or  rax,  (1 << 9)",        // CR4.OSFXSR = 1
            "or  rax,  (1 << 10)",       // CR4.OSXMMEXCPT = 1
            "mov cr4, rax"
        );

        // Pasar el puntero a la estructura de framebuffer en rdi (primer argumento de la ABI x86_64)
        // Asumimos que tienes una variable llamada framebuffer_info_ptr con la dirección física de la info del framebuffer
        core::arch::asm!(
            "mov cr3, {cr3}",           // Configurar paginación
            "mov rsp, {rsp}",           // Configurar stack
            "mov rdi, {fbinfo}",        // Pasar framebuffer_info_ptr en RDI
            "mov rax, {entry}",         // Copiar entry point a RAX
            "jmp rax",                  // Saltar directamente al kernel
            cr3 = in(reg) cr3_value,
            rsp = in(reg) rsp_alineado,
            fbinfo = in(reg) framebuffer_info_ptr,
            entry = in(reg) entry_reg,
            options(noreturn)
        );

        // Si llegamos aquí, el kernel retornó (no debería pasar)
        serial_write_str("BL: ERROR - kernel retorno!\r\n");
    }
}