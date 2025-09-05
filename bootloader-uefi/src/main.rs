#![no_std]
#![no_main]

use core::fmt::Write;
use core::slice;
use uefi::prelude::*;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, Directory, RegularFile, FileAttribute, FileInfo, FileMode};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::{AllocateType, MemoryType, BootServices};
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
            core::arch::asm!("hlt");
        }
    }
}

const KERNEL_PHYS_LOAD_ADDR: u64 = 0x0020_0034;

#[inline(always)]
fn pages_for_size(size: usize) -> usize { (size + 0xFFF) / 0x1000 }

fn open_root_fs(bs: &BootServices, image_handle: Handle) -> uefi::Result<Directory> {
    let image = bs.open_protocol_exclusive::<LoadedImage>(image_handle)?;
    let device_handle = image.device().expect("LoadedImage without device handle");
    let mut fs = bs.open_protocol_exclusive::<SimpleFileSystem>(device_handle)?;
    fs.open_volume()
}

fn open_kernel_file(root: &mut Directory) -> uefi::Result<RegularFile> {
    let candidates = [
        uefi::cstr16!("eclipse_kernel"),
        uefi::cstr16!("\\eclipse_kernel"),
        uefi::cstr16!("EFI\\BOOT\\eclipse_kernel"),
        uefi::cstr16!("\\EFI\\BOOT\\eclipse_kernel"),
        uefi::cstr16!("boot\\eclipse_kernel"),
        uefi::cstr16!("\\boot\\eclipse_kernel"),
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

unsafe fn jump_to_kernel(entry: u64, _framebuffer_info: FramebufferInfo) -> ! {
    // Llamar directamente al punto de entrada del kernel
    // El kernel manejará su propia inicialización
    let entry_fn: extern "C" fn() -> ! = core::mem::transmute(entry as usize);
    entry_fn()
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

const PT_LOAD: u32 = 1;

fn load_elf64_segments(bs: &BootServices, file: &mut RegularFile) -> Result<u64, BootError> {
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

    // PASO 1: calcular el rango total [min_start, max_end) de todos los PT_LOAD
    let mut min_start: u64 = u64::MAX;
    let mut max_end: u64 = 0;
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
        let dest_phys = if phdr.p_paddr != 0 { phdr.p_paddr } else { phdr.p_vaddr };
        let dest_start = dest_phys & !0xFFFu64;
        let dest_end = (dest_phys + phdr.p_memsz + 0xFFF) & !0xFFFu64;
        if dest_start < min_start { min_start = dest_start; }
        if dest_end > max_end { max_end = dest_end; }
    }
    if min_start == u64::MAX || max_end <= min_start { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }
    let total_pages = ((max_end - min_start) / 0x1000) as usize;
    if let Err(st) = bs.allocate_pages(AllocateType::Address(min_start), MemoryType::LOADER_CODE, total_pages) {
        return Err(BootError::LoadSegment { status: st.status(), seg_index: 0, addr: min_start, pages: total_pages });
    }

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

        let dest_phys = if phdr.p_paddr != 0 { phdr.p_paddr } else { phdr.p_vaddr };
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
        }
        // Cero para .bss
        if phdr.p_memsz > phdr.p_filesz {
            let zero_ptr = (dest_phys + phdr.p_filesz) as *mut u8;
            let zero_len = (phdr.p_memsz - phdr.p_filesz) as usize;
            unsafe { core::ptr::write_bytes(zero_ptr, 0, zero_len); }
        }
    }

    let entry = if ehdr.e_entry != 0 { ehdr.e_entry } else { 0x0020_0000 };
    Ok(entry)
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

fn prepare_boot_environment(bs: &BootServices, handle: Handle) -> core::result::Result<(u64, u64, u64), BootError> {
    // Abrir raíz del FS
    let mut root = open_root_fs(bs, handle).map_err(|e| BootError::OpenRoot(e.status()))?;

    // Abrir kernel
    let mut kernel_file = open_kernel_file(&mut root).map_err(|e| BootError::OpenKernel(e.status()))?;

    // Cargar ELF64 y obtener entry
    let entry = load_elf64_segments(bs, &mut kernel_file)?;

    // Reservar stack (64 KiB) por debajo de 1 GiB para que esté mapeada (identidad 0..1GiB)
    let stack_pages: usize = 16;
    let one_gib: u64 = 1u64 << 30;
    let max_stack_addr: u64 = one_gib - 0x1000; // límite superior < 1GiB
    let stack_base = bs
        .allocate_pages(AllocateType::MaxAddress(max_stack_addr), MemoryType::LOADER_DATA, stack_pages)
        .map_err(|e| BootError::AllocStack(e.status()))?;
    let stack_top = stack_base + (stack_pages as u64) * 4096u64;

    // Configurar paginación identidad simple (1 GiB, páginas de 2 MiB)
    // Allocate: 1 página para PML4, 1 para PDPT, 1 para PD
    let pml4_phys = bs
        .allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .map_err(|e| BootError::AllocPml4(e.status()))?;
    let pdpt_phys = bs
        .allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .map_err(|e| BootError::AllocPdpt(e.status()))?;
    let pd_phys = bs
        .allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .map_err(|e| BootError::AllocPd(e.status()))?;

        unsafe {
        // Limpiar tablas
        core::ptr::write_bytes(pml4_phys as *mut u8, 0, 4096);
        core::ptr::write_bytes(pdpt_phys as *mut u8, 0, 4096);
        core::ptr::write_bytes(pd_phys as *mut u8, 0, 4096);

        // Flags
        let p_w = 0x003u64; // Present | Write
        let ps = 0x080u64; // Page Size (2 MiB) en PDE

        // PML4[0] -> PDPT
        let pml4 = pml4_phys as *mut u64;
        *pml4 = (pdpt_phys & 0x000F_FFFF_FFFF_F000u64) | p_w;

        // PDPT[0] -> PD
        let pdpt = pdpt_phys as *mut u64;
        *pdpt = (pd_phys & 0x000F_FFFF_FFFF_F000u64) | p_w;

        // PD entries: identidad 0..1GiB con 2MiB páginas
        let pd = pd_phys as *mut u64;
        for i in 0..512u64 {
            let base = i * 0x20_0000u64; // 2 MiB
            *pd.add(i as usize) = (base & 0x000F_FFFF_FFFF_F000u64) | p_w | ps;
        }
    }

    Ok((entry, stack_top, pml4_phys))
}

#[entry]
fn main(handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // Serial init temprano
    unsafe { serial_init(); serial_write_str("BL: inicio\r\n"); }
    // Mensaje inicial
    {
        let mut out = system_table.stdout();
        let _ = out.write_str("Eclipse OS Bootloader UEFI\n");
        let _ = out.write_str("Cargando kernel ELF...\n");
    }

    // Preparación con reporte de error detallado
    let (entry_address, stack_top, pml4_phys): (u64, u64, u64) = {
        let bs = system_table.boot_services();
        match prepare_boot_environment(bs, handle) {
            Ok(tuple) => tuple,
            Err(err) => {
                let mut out = system_table.stdout();
                let _ = out.write_str("ERROR antes de cargar kernel: ");
                match err {
                    BootError::OpenRoot(st) => { let _ = out.write_str("open_root_fs "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                    BootError::OpenKernel(st) => { let _ = out.write_str("open_kernel_file "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                    BootError::LoadElf(st) => { let _ = out.write_str("load_elf64_segments "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                    BootError::LoadSegment { status, seg_index, addr, pages } => {
                        let _ = out.write_str("load_elf64_segments seg ");
                        let _ = core::fmt::write(&mut out, format_args!("{} ", seg_index));
                        let _ = core::fmt::write(&mut out, format_args!("addr=0x{:016x} pages={} status=", addr, pages));
                        let _ = core::fmt::write(&mut out, format_args!("{:?}", status));
                    }
                    BootError::AllocStack(st) => { let _ = out.write_str("allocate_pages stack "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                    BootError::AllocPml4(st) => { let _ = out.write_str("allocate_pages PML4 "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                    BootError::AllocPdpt(st) => { let _ = out.write_str("allocate_pages PDPT "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                    BootError::AllocPd(st) => { let _ = out.write_str("allocate_pages PD "); let _ = core::fmt::write(&mut out, format_args!("{:?}", st)); }
                }
                let _ = out.write_str("\n");
                loop { unsafe { core::arch::asm!("hlt"); } }
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
    };
    
    // Intentar obtener información del framebuffer usando Graphics Output Protocol
    {
        let bs = system_table.boot_services();
        // Buscar el protocolo GOP en todos los handles disponibles
        let mut gop_handle = None;
        let mut gop_protocol = None;
        
        // Obtener todos los handles
        if let Ok(handles) = bs.locate_handle_buffer(uefi::table::boot::SearchType::ByProtocol(&GraphicsOutput::GUID)) {
            for handle in handles.iter() {
                if let Ok(gop) = unsafe { 
                    bs.open_protocol::<GraphicsOutput>(
                        uefi::table::boot::OpenProtocolParams {
                            handle: *handle,
                            agent: *handle,
                            controller: None,
                        },
                        uefi::table::boot::OpenProtocolAttributes::GetProtocol,
                    )
                } {
                    gop_handle = Some(*handle);
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
            
            unsafe { 
                serial_write_str("BL: GOP encontrado\r\n");
                // Log de información del framebuffer
                let mut buf = [0u8; 32];
                let mut n = 0usize;
                
                // Log base_address
                unsafe { serial_write_str("BL: base=0x"); }
                for i in (0..16).rev() {
                    let nyb = ((framebuffer_info.base_address >> (i*4)) & 0xF) as u8;
                    buf[n] = if nyb < 10 { b'0'+nyb } else { b'a'+(nyb-10) }; n+=1;
                }
                buf[n] = b'\r'; n+=1; buf[n] = b'\n'; n+=1;
                unsafe { for i in 0..n { serial_write_byte(buf[i]); } }
                
                // Log resolución
                unsafe { 
                    serial_write_str("BL: res="); 
                    // Width
                    n = 0;
                    let w = framebuffer_info.width;
                    if w == 0 { buf[n] = b'0'; n+=1; } else {
                        let mut temp = w;
                        while temp > 0 {
                            buf[n] = b'0' + (temp % 10) as u8;
                            temp /= 10;
                            n += 1;
                        }
                    }
                    buf[n] = b'x'; n+=1;
                    // Height
                    let h = framebuffer_info.height;
                    if h == 0 { buf[n] = b'0'; n+=1; } else {
                        let mut temp = h;
                        while temp > 0 {
                            buf[n] = b'0' + (temp % 10) as u8;
                            temp /= 10;
                            n += 1;
                        }
                    }
                    buf[n] = b'\r'; n+=1; buf[n] = b'\n'; n+=1;
                    for i in 0..n { serial_write_byte(buf[i]); }
                }
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
        let _ = out.write_str("Kernel ELF cargado\n");
        let _ = out.write_str("Entry ELF: 0x");
        let _ = core::fmt::write(out, format_args!("{:016x}\n", entry_address));
        unsafe { serial_write_str("BL: entry="); }
        {
            // serial hex simple
            let mut buf = [0u8; 18];
            let mut n = 0usize;
            for i in (0..16).rev() {
                let nyb = ((entry_address >> (i*4)) & 0xF) as u8;
                buf[n] = if nyb < 10 { b'0'+nyb } else { b'a'+(nyb-10) }; n+=1;
            }
            buf[n] = b'\r'; n+=1; buf[n] = b'\n'; n+=1;
            unsafe { for i in 0..n { serial_write_byte(buf[i]); } }
        }
        // Volcado de los primeros 16 bytes en entry
        let _ = out.write_str("Bytes@entry: ");
            unsafe {
            let ptr = entry_address as *const u8;
            for i in 0..16 {
                let b = core::ptr::read_volatile(ptr.add(i));
                let _ = core::fmt::write(out, format_args!("{:02x}", b));
                if i != 15 { let _ = out.write_str(" "); }
            }
        }
        let _ = out.write_str("\n");
        let _ = out.write_str("Saliendo de Boot Services...\n");
        unsafe { serial_write_str("BL: antes ExitBootServices\r\n"); }
    }

    // ExitBootServices (uefi 0.25.0)
    let (_rt_st, _final_map) = unsafe { system_table.exit_boot_services(MemoryType::LOADER_DATA) };
    unsafe { serial_write_str("BL: despues ExitBootServices\r\n"); }
    
    // Cambiar a nuestras tablas y stack antes de saltar al kernel
    unsafe {
        // Cargar CR3 con la PML4 propia (identidad 0..1GiB)
        core::arch::asm!(
            "mov cr3, {0}",
            in(reg) pml4_phys,
            options(nostack, preserves_flags)
        );

        // Cambiar el puntero de pila a la parte alta del stack reservado
        core::arch::asm!(
            "mov rsp, {0}",
            in(reg) stack_top,
            options(nostack, preserves_flags)
        );

        serial_write_str("BL: CR3 y stack configurados\r\n");
        serial_write_str("BL: saltando al kernel\r\n");
        jump_to_kernel(entry_address, framebuffer_info)
    }
}