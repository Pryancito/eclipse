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
use core::mem;

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
const KERNEL_VIRT_BASE: u64 = 0x200000; // Dirección fija del kernel (non-PIE)
const MAX_KERNEL_ALLOCATION: u64 = 64 * 1024 * 1024; // 64 MiB como límite razonable para el kernel

/// Guardar entry point para evitar corrupción por llamadas a funciones
static mut SAVED_ENTRY_REG: u64 = 0;

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
    // NOTA: Agregamos eclipse_microkernel como primera opción
    let candidates = [
        // Nuevo microkernel
        uefi::cstr16!("eclipse_microkernel"),
        uefi::cstr16!("\\eclipse_microkernel"),
        uefi::cstr16!("\\EFI\\BOOT\\eclipse_microkernel"),
        uefi::cstr16!("\\boot\\eclipse_microkernel"),
        // Kernel anterior (compatibilidad)
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
    // Mirroring a buffer de log en memoria
    log_append_byte(b);
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

// Buffer de log en memoria para volcar a archivo UEFI antes de ExitBootServices
static mut LOG_BUF: [u8; 131072] = [0; 131072];
static mut LOG_LEN: usize = 0;

/// Agrega un byte al buffer de log.
/// 
/// # Safety
/// 
/// Esta función es unsafe porque accede a las variables globales mutables LOG_BUF y LOG_LEN.
/// El llamador debe asegurar que:
/// - No haya acceso concurrente a LOG_BUF o LOG_LEN desde múltiples hilos
/// - Esta función solo se llame en contexto de bootloader single-threaded
#[inline(always)]
unsafe fn log_append_byte(b: u8) {
    if LOG_LEN < LOG_BUF.len() {
        *LOG_BUF.as_mut_ptr().add(LOG_LEN) = b;
        LOG_LEN += 1;
    }
}

/// Agrega múltiples bytes al buffer de log.
/// 
/// # Safety
/// 
/// Esta función es unsafe porque:
/// - Accede a las variables globales mutables LOG_BUF y LOG_LEN
/// - Usa copy_nonoverlapping para copiar datos
/// 
/// El llamador debe asegurar que:
/// - No haya acceso concurrente a LOG_BUF o LOG_LEN
/// - Esta función solo se llame en contexto de bootloader single-threaded
/// - bytes apunte a memoria válida por al menos bytes.len() bytes
#[inline(always)]
unsafe fn log_append_bytes(bytes: &[u8]) {
    let avail = LOG_BUF.len().saturating_sub(LOG_LEN);
    let to_copy = core::cmp::min(avail, bytes.len());
    if to_copy > 0 {
        // SAFETY: 
        // - bytes.as_ptr() es válido para lectura de to_copy bytes (garantizado por bytes.len())
        // - LOG_BUF.as_mut_ptr().add(LOG_LEN) es válido para escritura de to_copy bytes
        //   (garantizado por la comprobación avail y to_copy <= avail)
        // - Las regiones no se solapan (LOG_BUF es estático, bytes viene de otro lugar)
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), LOG_BUF.as_mut_ptr().add(LOG_LEN), to_copy);
        LOG_LEN += to_copy;
    }
}

fn flush_log_to_file(bs: &BootServices, image_handle: Handle) {
    // Intentar abrir FS raíz y escribir \\log.txt
    if let Ok(mut root) = open_root_fs(bs, image_handle) {
        // Crear/truncar archivo
        if let Ok(file) = root.open(uefi::cstr16!("\\log.txt"), FileMode::CreateReadWrite, FileAttribute::empty()) {
            if let Some(mut reg) = file.into_regular_file() {
                // Reiniciar posición al inicio
                let _ = reg.set_position(0);
                // Escribir el buffer actual
                let len = unsafe { LOG_LEN };
                let data = unsafe { &LOG_BUF[..len] };
                let _ = reg.write(data);
            }
        }
    }
}

/// Mapear el framebuffer en direcciones altas con identidad (2MiB pages) en nuestras tablas
/// 
/// # Safety
/// 
/// Esta función manipula directamente las tablas de páginas de x86_64. Es unsafe porque:
/// - Asume que pml4_phys apunta a una tabla PML4 válida y alineada a 4KB
/// - Modifica estructuras de memoria críticas del sistema
/// - Usa aritmética de punteros sin límites explícitos
/// 
/// El llamador debe asegurar que:
/// - pml4_phys es una dirección física válida de una tabla PML4
/// - fb_base y fb_size son válidos y no causan desbordamiento
/// - Esta función se llama en contexto apropiado del bootloader antes de ExitBootServices
fn map_framebuffer_identity(bs: &BootServices, pml4_phys: u64, fb_base: u64, fb_size: u64) {
    // SAFETY: serial_write_str solo escribe a puertos serie, es seguro en contexto bootloader
    unsafe {
        serial_write_str("BL: mapeando framebuffer en page tables...\r\n");
    }
    let p_w: u64 = 0x003; // Present | Write
    let ps: u64 = 0x080;   // Page Size (2MiB)
    let addr_mask: u64 = 0x000F_FFFF_FFFF_F000u64;

    // Calcular límites alineados a 2MiB para evitar mapeos parciales
    let phys_start = fb_base & !0x1F_FFFFu64; // 2MiB aligned down
    
    // Validar que fb_base + fb_size no desborde
    let fb_end = fb_base.checked_add(fb_size)
        .unwrap_or_else(|| {
            // SAFETY: solo escribe a puerto serie
            unsafe { serial_write_str("BL: ERROR - framebuffer address overflow\r\n"); }
            // Si desborda, usar el valor máximo seguro
            u64::MAX - 0x1F_FFFFu64
        });
    
    let phys_end = fb_end.checked_add(0x1F_FFFFu64)
        .map(|v| v & !0x1F_FFFFu64)
        .unwrap_or_else(|| {
            // Si desborda, usar el máximo alineado posible
            u64::MAX & !0x1F_FFFFu64
        });

    // SAFETY: Asumimos que pml4_phys apunta a memoria válida alineada a 4KB
    // Esta es una precondición de la función
    let pml4_ptr = pml4_phys as *mut u64;

    let mut addr = phys_start;
    while addr < phys_end {
        // Calcular índice PML4 (bits 39-47)
        let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
        
        // SAFETY: pml4_idx está limitado a 0-511 por el mask 0x1FF, por lo que
        // pml4_ptr.add(pml4_idx) siempre está dentro de los límites de la tabla PML4 (512 entradas)
        let pdpt_entry = unsafe { pml4_ptr.add(pml4_idx) };
        
        // SAFETY: pdpt_entry apunta a una entrada válida de la tabla PML4
        let mut pdpt_phys: u64 = unsafe { *pdpt_entry } & addr_mask;
        
        if pdpt_phys == 0 {
            // Necesitamos crear una nueva tabla PDPT
            if let Ok(new_pdpt) = bs.allocate_pages(AllocateType::AnyPages, MemoryType::BOOT_SERVICES_DATA, 1) {
                // SAFETY: new_pdpt es memoria recién asignada de 4096 bytes por UEFI
                unsafe { core::ptr::write_bytes(new_pdpt as *mut u8, 0, 4096); }
                pdpt_phys = new_pdpt & addr_mask;
                // SAFETY: pdpt_entry apunta a entrada válida en PML4
                unsafe { *pdpt_entry = pdpt_phys | p_w; }
            } else {
                // SAFETY: solo escribe a puerto serie
                unsafe { serial_write_str("BL: ERROR alloc PDPT\r\n"); }
                break;
            }
        }

        // SAFETY: pdpt_phys ahora contiene una dirección válida de tabla PDPT
        let pdpt_ptr = pdpt_phys as *mut u64;
        let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
        
        // SAFETY: pdpt_idx está limitado a 0-511, dentro de límites de PDPT
        let pd_entry = unsafe { pdpt_ptr.add(pdpt_idx) };
        
        // SAFETY: pd_entry apunta a entrada válida de PDPT
        let mut pd_phys: u64 = unsafe { *pd_entry } & addr_mask;
        
        if pd_phys == 0 {
            // Necesitamos crear una nueva tabla PD
            if let Ok(new_pd) = bs.allocate_pages(AllocateType::AnyPages, MemoryType::BOOT_SERVICES_DATA, 1) {
                // SAFETY: new_pd es memoria recién asignada de 4096 bytes por UEFI
                unsafe { core::ptr::write_bytes(new_pd as *mut u8, 0, 4096); }
                pd_phys = new_pd & addr_mask;
                // SAFETY: pd_entry apunta a entrada válida en PDPT
                unsafe { *pd_entry = pd_phys | p_w; }
            } else {
                // SAFETY: solo escribe a puerto serie
                unsafe { serial_write_str("BL: ERROR alloc PD\r\n"); }
                break;
            }
        }

        // SAFETY: pd_phys ahora contiene una dirección válida de tabla PD
        let pd_ptr = pd_phys as *mut u64;
        let pd_idx = ((addr >> 21) & 0x1FF) as usize;
        
        // SAFETY: pd_idx está limitado a 0-511, dentro de límites de PD
        // Escribimos una entrada de página de 2MiB con mapeo de identidad
        unsafe {
            *pd_ptr.add(pd_idx) = (addr & addr_mask) | p_w | ps;
        }

        // Avanzar al siguiente bloque de 2MiB, usando saturating_add para prevenir overflow
        addr = addr.saturating_add(0x20_0000u64); // next 2MiB
    }
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
    // SAFETY: ehdr_buf está completamente inicializado con datos del archivo ELF.
    // Usamos read_unaligned porque la alineación de la estructura puede no coincidir con el buffer.
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
    
    // Validar que phnum no sea excesivamente grande para prevenir ataques
    if phnum > MAX_PH {
        return Err(BootError::LoadElf(Status::UNSUPPORTED));
    }
    
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
        // SAFETY: ph_buf está completamente inicializado con phentsize bytes del archivo ELF.
        // Usamos read_unaligned para evitar problemas de alineación.
        let phdr: Elf64Phdr = unsafe { core::ptr::read_unaligned(ph_buf.as_ptr() as *const Elf64Phdr) };
        if phdr.p_type != PT_LOAD { continue; }

        // Validar que p_memsz no cause desbordamiento al sumarse con p_vaddr
        let vaddr_end_unaligned = phdr.p_vaddr.checked_add(phdr.p_memsz)
            .ok_or(BootError::LoadElf(Status::LOAD_ERROR))?;
        
        // Calcular direcciones virtuales del segmento con alineación de página
        let vaddr_start = phdr.p_vaddr & !0xFFFu64;
        // Validar que la suma no desborde antes de alinear
        let vaddr_end = vaddr_end_unaligned.checked_add(0xFFF)
            .map(|v| v & !0xFFFu64)
            .ok_or(BootError::LoadElf(Status::LOAD_ERROR))?;

        if vaddr_start < min_vaddr { min_vaddr = vaddr_start; }
        if vaddr_end > max_vaddr { max_vaddr = vaddr_end; }
    }

    if min_vaddr == u64::MAX || max_vaddr <= min_vaddr { return Err(BootError::LoadElf(Status::LOAD_ERROR)); }

    // Calcular tamaño total del kernel con verificación de desbordamiento
    let total_size = max_vaddr.checked_sub(min_vaddr)
        .ok_or(BootError::LoadElf(Status::LOAD_ERROR))?;
    
    // Validar que el tamaño no exceda un límite razonable
    if total_size > MAX_KERNEL_ALLOCATION {
        return Err(BootError::LoadElf(Status::LOAD_ERROR));
    }
    
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

        // Calcular direcciones virtuales del segmento con overflow protection
        let vaddr_start = phdr.p_vaddr & !0xFFFu64;
        
        // Reutilizar el mismo patrón de validación que en el primer pase
        let vaddr_end_unaligned = phdr.p_vaddr.checked_add(phdr.p_memsz)
            .ok_or(BootError::LoadElf(Status::LOAD_ERROR))?;
        
        let vaddr_end = vaddr_end_unaligned.checked_add(0xFFF)
            .map(|v| v & !0xFFFu64)
            .ok_or(BootError::LoadElf(Status::LOAD_ERROR))?;

        if seg_idx < MAX_PH {
            segmap.vstart[seg_idx] = vaddr_start;
            
            // Calcular dirección física con overflow protection
            let offset = vaddr_start.checked_sub(min_vaddr)
                .ok_or(BootError::LoadElf(Status::LOAD_ERROR))?;
            
            segmap.pstart[seg_idx] = kernel_phys_base.checked_add(offset)
                .ok_or(BootError::LoadElf(Status::LOAD_ERROR))?;
            
            segmap.len[seg_idx] = vaddr_end.checked_sub(vaddr_start)
                .ok_or(BootError::LoadElf(Status::LOAD_ERROR))?;
            
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

fn load_kernel_from_data(bs: &BootServices, kernel_data: &[u8]) -> Result<(u64, u64, u64, u64), BootError> {
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

    // Calcular tamaño total necesario basándose en el rango real de offsets
    let mut max_offset: u64 = 0;

    for i in 0..phnum {
        let off = ehdr.e_phoff + (i as u64) * (ehdr.e_phentsize as u64);
        if off as usize + phentsize > kernel_data.len() {
            return Err(BootError::LoadElf(Status::LOAD_ERROR));
        }

        let phdr: Elf64Phdr = unsafe {
            core::ptr::read_unaligned(kernel_data[off as usize..].as_ptr() as *const Elf64Phdr)
        };

        if phdr.p_type != PT_LOAD { continue; }
        if phdr.p_memsz == 0 { continue; }

        // Eliminar verificación de KERNEL_VIRT_BASE - aceptar cualquier dirección para kernel PIE
        // if phdr.p_vaddr < KERNEL_VIRT_BASE {
        //     unsafe {
        //         serial_write_str("DEBUG: Segmento con vaddr por debajo de KERNEL_VIRT_BASE, ignorado\r\n");
        //     }
        //     continue;
        // }

        let seg_end = phdr.p_vaddr.checked_add(phdr.p_memsz).ok_or(BootError::LoadElf(Status::LOAD_ERROR))?;
        if seg_end <= KERNEL_VIRT_BASE { continue; }

        let offset_end = seg_end - KERNEL_VIRT_BASE;
        if offset_end > max_offset {
            max_offset = offset_end;
        }
    }

    if max_offset == 0 {
        return Err(BootError::LoadElf(Status::LOAD_ERROR));
    }

    let mut total_size = (max_offset + 0xFFF) & !0xFFF; // alinear hacia arriba a página
    if total_size > MAX_KERNEL_ALLOCATION {
        unsafe {
            serial_write_str("DEBUG: total_size excede MAX_KERNEL_ALLOCATION, truncando\r\n");
        }
        total_size = MAX_KERNEL_ALLOCATION;
    }

    let total_pages = ((total_size + 0xFFF) / 0x1000) as usize;
    
    // Debug: mostrar el tamaño calculado
    unsafe {
        serial_write_str("DEBUG: total_size calculado: 0x");
        serial_write_hex64(total_size);
        serial_write_str(" (");
        serial_write_hex64(total_size);
        serial_write_str(" bytes)\r\n");
        serial_write_str("DEBUG: total_pages calculado: ");
        serial_write_hex64(total_pages as u64);
        serial_write_str("\r\n");
    }

    // Reservar memoria física (usar AnyPages para evitar conflictos de dirección)
    // Reservar memoria física para el kernel
    // IMPORTANTE: Necesitamos alineación de 2MB para Huge Pages.
    // AllocatePages(AnyPages) solo garantiza 4KB.
    // Estrategia: Reservar size + 2MB extra y alinear manualmente.
    
    let extra_pages_for_alignment = 512; // 2MB / 4KB
    let allocation_pages = total_pages + extra_pages_for_alignment;
    
    let mut allocated_addr;
    
    match bs.allocate_pages(AllocateType::AnyPages, MemoryType::BOOT_SERVICES_CODE, allocation_pages) {
        Ok(addr) => {
            // Calcular dirección alineada a 2MB
            let addr_u64 = addr;
            let alignment = 0x200000;
            let aligned_addr = (addr_u64 + (alignment - 1)) & !(alignment - 1);
            
            allocated_addr = aligned_addr;
            
            unsafe {
                serial_write_str("DEBUG: Kernel alloc raw: 0x");
                serial_write_hex64(addr);
                serial_write_str(" aligned: 0x");
                serial_write_hex64(allocated_addr);
                serial_write_str("\r\n");
            }
        },
        Err(st_any) => {
             return Err(BootError::LoadSegment { status: st_any.status(), seg_index: 0, addr: kernel_phys_base, pages: allocation_pages });
        }
    }

    // Cargar segmentos
    unsafe {
        serial_write_str("DEBUG: Iniciando carga de segmentos...\r\n");
    }
    
    for i in 0..phnum {
        let off = ehdr.e_phoff + (i as u64) * (ehdr.e_phentsize as u64);
        let phdr: Elf64Phdr = unsafe {
            core::ptr::read_unaligned(kernel_data[off as usize..].as_ptr() as *const Elf64Phdr)
        };

        if phdr.p_type != PT_LOAD { 
            unsafe {
                serial_write_str("DEBUG: Segmento ");
                serial_write_hex64(i as u64);
                serial_write_str(" no es PT_LOAD, saltando\r\n");
            }
            continue; 
        }
        
        unsafe {
            serial_write_str("DEBUG: Cargando segmento ");
            serial_write_hex64(i as u64);
            serial_write_str("...\r\n");
        }

        // Calcular dirección física donde cargar el segmento
        // Para kernel PIE: usar p_vaddr directamente como offset desde la base de carga
        // Para kernel con dirección fija: restar KERNEL_VIRT_BASE
        let offset = if KERNEL_VIRT_BASE == 0x0 {
            phdr.p_vaddr // PIE: usar vaddr directamente
        } else {
            phdr.p_vaddr.checked_sub(KERNEL_VIRT_BASE).unwrap_or(0) // Fixed: restar base
        };
        let dest_phys = allocated_addr + offset;
        
        unsafe {
            serial_write_str("DEBUG: Segmento ");
            serial_write_hex64(i as u64);
            serial_write_str(" - p_vaddr=0x");
            serial_write_hex64(phdr.p_vaddr);
            serial_write_str(" dest_phys=0x");
            serial_write_hex64(dest_phys);
            serial_write_str(" offset=0x");
            serial_write_hex64(offset);
            serial_write_str(" p_memsz=0x");
            serial_write_hex64(phdr.p_memsz);
            serial_write_str("\r\n");
        }

        // Cargar datos del segmento (limitar el tamaño para evitar segmentos excesivos)
        let available = total_size.saturating_sub(offset);
        let mut actual_filesz = phdr.p_filesz;
        if actual_filesz > available { actual_filesz = available; }
        
        if actual_filesz > 0 {
            if phdr.p_offset as usize + actual_filesz as usize > kernel_data.len() {
                return Err(BootError::LoadElf(Status::LOAD_ERROR));
            }

            let src_data = &kernel_data[phdr.p_offset as usize..phdr.p_offset as usize + actual_filesz as usize];
            let dst_ptr = dest_phys as *mut u8;
            
            unsafe {
                serial_write_str("DEBUG: Copiando ");
                serial_write_hex64(actual_filesz);
                serial_write_str(" bytes de 0x");
                serial_write_hex64(src_data.as_ptr() as u64);
                serial_write_str(" a 0x");
                serial_write_hex64(dst_ptr as u64);
                serial_write_str("\r\n");
                
                core::ptr::copy_nonoverlapping(src_data.as_ptr(), dst_ptr, actual_filesz as usize);
                
                serial_write_str("DEBUG: Copia completada\r\n");
            }
        }

        // Inicializar memoria .bss (ceros) - limitar al espacio reservado
        let mut actual_memsz = phdr.p_memsz;
        if actual_memsz > available { actual_memsz = available; }
        
        if actual_memsz > actual_filesz {
            let bss_ptr = (dest_phys + actual_filesz) as *mut u8;
            let bss_len = (actual_memsz - actual_filesz) as usize;
            unsafe { core::ptr::write_bytes(bss_ptr, 0, bss_len); }
        }
    }

    // Calcular la dirección física del entry point
    // El kernel se carga en allocated_addr, y el entry point virtual usa KERNEL_VIRT_BASE
    // Necesitamos convertir la dirección virtual a física
    let entry_point_phys = if ehdr.e_entry != 0 {
        let entry_offset = ehdr.e_entry.checked_sub(KERNEL_VIRT_BASE).unwrap_or(0);
        let calculated = allocated_addr + entry_offset;
        unsafe {
            serial_write_str("DEBUG: Entry point virtual: 0x");
            serial_write_hex64(ehdr.e_entry);
            serial_write_str("\r\n");
            serial_write_str("DEBUG: Memoria asignada en: 0x");
            serial_write_hex64(allocated_addr);
            serial_write_str("\r\n");
            serial_write_str("DEBUG: Offset entry calculado: 0x");
            serial_write_hex64(entry_offset);
            serial_write_str("\r\n");
            serial_write_str("DEBUG: Entry point físico calculado: 0x");
            serial_write_hex64(calculated);
            serial_write_str("\r\n");
        }
        calculated
    } else {
        unsafe {
            serial_write_str("DEBUG: ehdr.e_entry es 0, usando fallback\r\n");
        }
        allocated_addr // fallback
    };
    
    // Debug: mostrar el entry point calculado
    unsafe {
        serial_write_str("DEBUG: entry point calculado: 0x");
        serial_write_hex64(entry_point_phys);
        serial_write_str(" (VA: 0x");
        serial_write_hex64(ehdr.e_entry);
        serial_write_str(", PA base: 0x");
        serial_write_hex64(allocated_addr);
        serial_write_str(")\r\n");
        
        // Debug crítico: verificar que el entry point no sea 0xB0000
        if entry_point_phys == 0xB0000 {
            serial_write_str("DEBUG: ERROR CRITICO - entry_point_phys es 0xB0000!\r\n");
            serial_write_str("DEBUG: ehdr.e_entry = 0x");
            serial_write_hex64(ehdr.e_entry);
            serial_write_str("\r\n");
            serial_write_str("DEBUG: allocated_addr = 0x");
            serial_write_hex64(allocated_addr);
            serial_write_str("\r\n");
            serial_write_str("DEBUG: Calculo: allocated_addr + (ehdr.e_entry - 0x200060)\r\n");
            serial_write_str("DEBUG: Calculo: 0x");
            serial_write_hex64(allocated_addr);
            serial_write_str(" + (0x");
            serial_write_hex64(ehdr.e_entry);
            serial_write_str(" - 0x200060)\r\n");
        }
    };

    Ok((entry_point_phys, ehdr.e_entry, allocated_addr, total_size))
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

    // Configurar paginación identidad extendida (64 GiB, páginas de 2 MiB) para cubrir direcciones altas del kernel y VirtIO
    // Allocate: 1 página para PML4, 2 para PDPT (para cubrir 512 GiB cada uno), 64 para PD (1 por GiB)
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

    // 64 PD para cubrir 64 GiB (incluyendo direcciones VirtIO altas)
    let mut pd_phys_arr: [u64; 64] = [0; 64];
    for gi_b in 0..64 {
        pd_phys_arr[gi_b] = bs
            .allocate_pages(AllocateType::AnyPages, MemoryType::BOOT_SERVICES_DATA, 1)
            .map_err(|e| BootError::AllocPd(e.status()))?;
    }

        unsafe {
        // Limpiar tablas
        core::ptr::write_bytes(pml4_phys as *mut u8, 0, 4096);
        core::ptr::write_bytes(pdpt_phys as *mut u8, 0, 4096);
        core::ptr::write_bytes(pdpt_phys_high as *mut u8, 0, 4096);
        for gi_b in 0..64 { core::ptr::write_bytes(pd_phys_arr[gi_b] as *mut u8, 0, 4096); }

        // Flags
        let p_w = 0x003u64; // Present | Write
        let ps = 0x080u64; // Page Size (2 MiB) en PDE

        // PML4[0] -> PDPT
        let pml4 = pml4_phys as *mut u64;
        *pml4.add(0) = (pdpt_phys & 0x000F_FFFF_FFFF_F000u64) | p_w;

        // PDPT[0..63] -> PDs (cada entrada mapea 1 GiB) - Primeros 64 GiB en el primer PDPT
        let pdpt = pdpt_phys as *mut u64;
        for gi_b in 0..64u64 {
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

        // El segundo PDPT no se usa ahora - todo está en el primero
        serial_write_str("BL: DEBUG - Todo mapeado en primer PDPT (0-64 GiB)\r\n");
        
        // Debug específico para verificar mapeo de 32 GiB
        let pdpt = pdpt_phys as *mut u64;
        let entry_32 = unsafe { *pdpt.add(32) };
        serial_write_str("BL: DEBUG - Verificando PDPT[32] (32 GiB)\r\n");
        serial_write_hex64(entry_32);
        serial_write_str("\r\n");
        
        if entry_32 != 0 {
            let pd_addr = entry_32 & 0x000F_FFFF_FFFF_F000u64;
            serial_write_str("BL: DEBUG - PD físico: ");
            serial_write_hex64(pd_addr);
            serial_write_str("\r\n");
        } else {
            serial_write_str("BL: ERROR - PDPT[32] está vacío!\r\n");
        }

        // Conectar el segundo PDPT al PML4 en la entrada 1 (para direcciones >= 512 GiB)
        *pml4.add(1) = (pdpt_phys_high & 0x000F_FFFF_FFFF_F000u64) | p_w;
        
        serial_write_str("BL: DEBUG - PML4[0] apunta a PDPT (0-64 GiB)\r\n");
        serial_write_str("BL: DEBUG - VirtIO 0x800000000 (32 GiB) está en PDPT[32]\r\n");

        // Mapear framebuffer si es necesario (dirección típica 0x80000000+)
        // Nota: framebuffer_info no está disponible aquí, se maneja después

        // Nota: mantenemos mapeo identidad 0–64 GiB (incluyendo VirtIO en 0x800000000+)
        serial_write_str("BL: DEBUG - Mapeo de identidad extendido configurado para 64 GiB\r\n");
        serial_write_str("BL: DEBUG - VirtIO debería poder acceder a 0x800000000+\r\n");
    }

    unsafe { serial_write_str("DEBUG: prepare_page_tables_only completed\r\n"); }
    Ok((pml4_phys, stack_top))
}

/// Mapea la dirección virtual del kernel a su dirección física real
/// Debe ser llamado DESPUÉS de cargar el kernel para conocer su dirección física
fn map_kernel_virtual_to_physical(pml4_phys: u64, kernel_virt: u64, kernel_phys: u64, kernel_size: u64) {
    unsafe {
        serial_write_str("BL: Mapeando kernel VA 0x");
        serial_write_hex64(kernel_virt);
        serial_write_str(" -> PA 0x");
        serial_write_hex64(kernel_phys);
        serial_write_str(" (tamaño: 0x");
        serial_write_hex64(kernel_size);
        serial_write_str(")\r\n");
    }
    
    let p_w: u64 = 0x003; // Present | Write
    let ps: u64 = 0x080;   // Page Size (2MiB)
    let addr_mask: u64 = 0x000F_FFFF_FFFF_F000u64;
    
    // Alinear a 2MB
    let virt_start = kernel_virt & !0x1F_FFFFu64;
    let phys_start = kernel_phys & !0x1F_FFFFu64;
    let virt_end = (kernel_virt + kernel_size + 0x1F_FFFFu64) & !0x1F_FFFFu64;
    
    let pml4_ptr = pml4_phys as *mut u64;
    
    let mut virt_addr = virt_start;
    let mut phys_addr = phys_start;
    
    while virt_addr < virt_end {
        let pml4_idx = ((virt_addr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((virt_addr >> 30) & 0x1FF) as usize;
        let pd_idx = ((virt_addr >> 21) & 0x1FF) as usize;
        
        unsafe {
            // Obtener o crear PDPT
            let pdpt_entry = pml4_ptr.add(pml4_idx);
            let pdpt_phys = if *pdpt_entry & 0x1 != 0 {
                *pdpt_entry & addr_mask
            } else {
                // Ya debería existir del mapeo de identidad, pero verificamos
                serial_write_str("BL: ERROR - PDPT no existe para mapeo del kernel!\r\n");
                return;
            };
            
            // Obtener o crear PD
            let pdpt_ptr = pdpt_phys as *mut u64;
            let pd_entry = pdpt_ptr.add(pdpt_idx);
            let pd_phys = if *pd_entry & 0x1 != 0 {
                *pd_entry & addr_mask
            } else {
                serial_write_str("BL: ERROR - PD no existe para mapeo del kernel!\r\n");
                return;
            };
            
            // Mapear en PD
            let pd_ptr = pd_phys as *mut u64;
            *pd_ptr.add(pd_idx) = (phys_addr & addr_mask) | p_w | ps;
        }
        
        virt_addr += 0x20_0000; // 2MB
        phys_addr += 0x20_0000; // 2MB
    }
    
    unsafe {
        serial_write_str("BL: Mapeo del kernel completado\r\n");
    }
}

fn load_eclipsefs_data(image: uefi::Handle, st: &mut SystemTable<Boot>) -> Result<(u64, u64), &'static str> {
    let mut fs = st.boot_services().get_image_file_system(image).expect("Failed to get file system");
    let mut root = fs.open_volume().expect("Failed to open volume");

    let mut file = match root.open(cstr16!("eclipsefs.img"), FileMode::Read, FileAttribute::empty()) {
        Ok(f) => f.into_regular_file().unwrap(), // Convertir a RegularFile
        Err(_) => return Err("Could not open eclipsefs.img"),
    };

    // Buffer para FileInfo. 128 bytes es suficiente.
    let mut info_buffer = [0u8; 128]; 
    let file_info = file.get_info::<FileInfo>(&mut info_buffer).expect("Failed to get file info");
    let file_size = file_info.file_size() as usize;

    let pages = (file_size + 0xFFF) / 0x1000;
    let mem_start = st.boot_services()
        .allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages)
        .expect("Failed to allocate pages for eclipsefs.img");
    
    let buf = unsafe { slice::from_raw_parts_mut(mem_start as *mut u8, file_size) };
    file.read(buf).expect("Failed to read eclipsefs.img");

    Ok((mem_start as u64, file_size as u64))
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
    // Copiar también al buffer de log (en caso de que el puerto serie no esté visible)
    unsafe { log_append_bytes(b"[BL] boot start\r\n"); }
    let out = system_table.stdout();
    let _ = out.write_str("DEBUG CONSOLE: about to call prepare_boot_environment\r\n");
    // Preparar solo las tablas de páginas ANTES de exit_boot_services
    let (pml4_phys, stack_top): (u64, u64) = {
        let bs = system_table.boot_services();
        unsafe { serial_write_str("DEBUG: got boot services\r\n"); }
        match prepare_page_tables_only(bs, handle) {
            Ok((pml4, stack)) => {
                unsafe { serial_write_str("DEBUG: prepare_page_tables_only returned OK\r\n"); }
                unsafe { log_append_bytes(b"[BL] pgtables ok\r\n"); }
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
                log_append_bytes(b"[BL] GOP ok\r\n");
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
                
                // Limpiar pantalla con negro y mostrar mensaje de inicio
                serial_write_str("BL: Limpiando pantalla y mostrando mensaje...\r\n");
                let fb_ptr = framebuffer_info.base_address as *mut u32;
                let stride = framebuffer_info.pixels_per_scan_line;
                
                // Limpiar pantalla con negro (0x00000000)
                for y in 0..framebuffer_info.height {
                    for x in 0..framebuffer_info.width {
                        let offset = (y * stride + x) as isize;
                        core::ptr::write_volatile(fb_ptr.offset(offset), 0x00000000);
                    }
                }
                
                serial_write_str("BL: Pantalla limpiada y mensaje mostrado\r\n");
            }
        } else {
            unsafe { 
                serial_write_str("BL: GOP no encontrado, usando VGA\r\n");
                log_append_bytes(b"[BL] GOP not found\r\n");
            }
        }
    }
    // Logs de depuración ANTES de salir de Boot Services
    {
        use core::fmt::Write as _;
        let out = system_table.stdout();
        let _ = out.write_str("Iniciando Sistema Operativo Eclipse v0.1.0\n");
        let _ = out.write_str("\n");
        let _ = out.write_str("Pasando el control al kernel...\n");
        unsafe { serial_write_str("BL: antes ExitBootServices\r\n"); }
    }

    // CARGAR EL KERNEL ANTES DE EXIT_BOOT_SERVICES Y SALTAR INMEDIATAMENTE
    // No almacenar entry_point en variables que puedan corromperse
    let mut kernel_entry_phys: u64 = 0;
    let mut kernel_base: u64 = 0;
    let mut kernel_size: u64 = 0;
    
    {
        let bs = system_table.boot_services();
        let kernel_data = include_bytes!("../../eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel");

        match load_kernel_from_data(bs, kernel_data) {
            Ok((entry_point_phys, entry_point_virt, kernel_phys_base, total_len)) => {
                unsafe {
                    serial_write_str("BL: kernel ELF cargado exitosamente\r\n");
                    log_append_bytes(b"[BL] kernel loaded\r\n");
                    serial_write_str("BL: entry_point_phys=0x");
                    serial_write_hex64(entry_point_phys);
                    serial_write_str(" entry_point_virt=0x");
                    serial_write_hex64(entry_point_virt);
                    serial_write_str(" kernel_phys_base=0x");
                    serial_write_hex64(kernel_phys_base);
                    serial_write_str(" total_len=0x");
                    serial_write_hex64(total_len);
                    serial_write_str("\r\n");
                }
                
                // La función load_kernel_from_data ya devuelve la dirección física del entry point
                // Mapear la dirección virtual del kernel (0x200000) a su dirección física real
                map_kernel_virtual_to_physical(pml4_phys, 0x200000, kernel_phys_base, total_len);
                
                // Asignar direcamente el Virtual Entry Point para el salto
                kernel_entry_phys = entry_point_virt;
                kernel_base = kernel_phys_base;
                kernel_size = total_len;
                
                // DEBUG INMEDIATO: verificar que se asignó correctamente
                unsafe {
                    serial_write_str("DEBUG: kernel_entry_phys ASIGNADO = 0x");
                    serial_write_hex64(kernel_entry_phys);
                    serial_write_str("\r\n");
                }
            },
            Err(e) => {
                unsafe {
                    serial_write_str("BL: ERROR cargando kernel ELF: ");
                    log_append_bytes(b"[BL] kernel load ERROR\r\n");
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
                kernel_entry_phys = 0x200000;
                kernel_base = KERNEL_PHYS_LOAD_ADDR;
                kernel_size = 0;
            }
        }
    }

    // Volcar log a archivo antes de ExitBootServices y mapear framebuffer
    {
        let bs = system_table.boot_services();
        // Mapear framebuffer detectado para que siga accesible tras cambiar CR3
        if framebuffer_info.base_address != 0 && framebuffer_info.width > 0 && framebuffer_info.height > 0 {
            let fb_bytes = (framebuffer_info.height as u64)
                .saturating_mul(framebuffer_info.pixels_per_scan_line as u64)
                .saturating_mul(4);
            map_framebuffer_identity(bs, pml4_phys, framebuffer_info.base_address, fb_bytes);
        }
        flush_log_to_file(bs, handle);
    }

    // ExitBootServices (uefi 0.25.0)
    // IMPORTANTE: Usar BOOT_SERVICES_CODE para mantener la memoria del kernel intacta
    let (_rt_st, _final_map) = system_table.exit_boot_services(MemoryType::BOOT_SERVICES_CODE);
    unsafe { serial_write_str("BL: despues ExitBootServices\r\n"); }
    // Nota: Framebuffer mapping simplificado por ahora

    // Configurar paginación identidad y pila y saltar al kernel (sin usar la pila después de cambiarla)
    unsafe {

        // DEBUG: Verificar estado antes del salto
        serial_write_str("BL: DEBUG antes del salto:\r\n");
        serial_write_str("BL: kernel_entry_phys=");
        serial_write_hex64(kernel_entry_phys);
        serial_write_str(" pml4_phys=");
        serial_write_hex64(pml4_phys);
        serial_write_str(" stack_top=");
        serial_write_hex64(stack_top);
        serial_write_str("\r\n");

        // Verificar que el código esté en la dirección física correcta
        serial_write_str("BL: verificando kernel en PA ");
        serial_write_hex64(kernel_base);
        serial_write_str(": ");
        let kernel_ptr = kernel_base as *const u8;
        for i in 0..16 {
            let byte = unsafe { *kernel_ptr.add(i) };
            serial_write_hex8(byte);
            serial_write_str(" ");
        }
        serial_write_str("\r\n");

        // DEBUG: Mostrar qué va a pasar justo antes del assembly
        serial_write_str("BL: saltando a kernel entry point...\r\n");
        
        // Verificar que el kernel esté realmente cargado ANTES del salto
        serial_write_str("BL: verificando kernel ANTES del salto: ");
        let kernel_check_ptr = kernel_base as *const u8;
        for i in 0..32 {
            let byte = unsafe { *kernel_check_ptr.add(i) };
            serial_write_hex8(byte);
            if i % 8 == 7 { serial_write_str(" "); }
        }
        serial_write_str("\r\n");

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

        // DEBUG VISUAL DEL BOOTLOADER - RECTÁNGULOS DE COLORES PARA DIAGNÓSTICO
        if framebuffer_info_ptr != 0 {
            let fb_info = unsafe { core::ptr::read_volatile(framebuffer_info_ptr as *const FramebufferInfo) };
            if fb_info.base_address != 0 {
                let fb_ptr = fb_info.base_address as *mut u32;
                let stride = fb_info.pixels_per_scan_line; // Usar stride en lugar de width
                
                // DEBUG SERIAL: Valores críticos antes del salto
                serial_write_str("BL: DEBUG CRITICO - Valores antes del salto:\r\n");
                serial_write_str("BL: CR3 (PML4): ");
                serial_write_hex64(cr3_value);
                serial_write_str("\r\nBL: RSP (Stack): ");
                serial_write_hex64(rsp_alineado);
                serial_write_str("\r\nBL: Entry Point: ");
                serial_write_hex64(kernel_entry_phys);
                serial_write_str("\r\nBL: Framebuffer Info: ");
                serial_write_hex64(framebuffer_info_ptr);
                
                // DEBUG DETALLADO DEL FRAMEBUFFER
                serial_write_str("\r\nBL: FRAMEBUFFER DETALLADO:\r\n");
                serial_write_str("BL: Base Address: ");
                serial_write_hex64(fb_info.base_address);
                serial_write_str("\r\nBL: Width: ");
                serial_write_hex64(fb_info.width as u64);
                serial_write_str("\r\nBL: Height: ");
                serial_write_hex64(fb_info.height as u64);
                serial_write_str("\r\nBL: Pixels per Scan Line: ");
                serial_write_hex64(fb_info.pixels_per_scan_line as u64);
                serial_write_str("\r\nBL: Pixel Format: ");
                serial_write_hex64(fb_info.pixel_format as u64);
                
                serial_write_str("\r\nBL: Ejecutando salto al kernel...\r\n");
            }
        }

        // DEBUG SERIAL: Justo antes de las instrucciones de ensamblador
        serial_write_str("BL: EJECUTANDO INSTRUCCIONES DE ENSAMBLADOR...\r\n");
        serial_write_str("BL: Configurando CR3, RSP, RDI, RAX...\r\n");
        
        // DEBUG DEL ESTADO DE LA CPU
        serial_write_str("BL: ESTADO DE LA CPU ANTES DEL SALTO:\r\n");
        
        // Leer CR0
        let cr0_val: u64;
        unsafe {
            core::arch::asm!("mov {}, cr0", out(reg) cr0_val);
        }
        serial_write_str("BL: CR0: ");
        serial_write_hex64(cr0_val);
        serial_write_str("\r\n");
        
        // Leer CR4
        let cr4_val: u64;
        unsafe {
            core::arch::asm!("mov {}, cr4", out(reg) cr4_val);
        }
        serial_write_str("BL: CR4: ");
        serial_write_hex64(cr4_val);
        serial_write_str("\r\n");
        
        // Leer CR3 actual
        let cr3_current: u64;
        unsafe {
            core::arch::asm!("mov {}, cr3", out(reg) cr3_current);
        }
        serial_write_str("BL: CR3 actual: ");
        serial_write_hex64(cr3_current);
        serial_write_str("\r\n");
        
        // Debug adicional del entry point
        serial_write_str("BL: Entry point calculado: 0x");
        serial_write_hex64(kernel_entry_phys);
        serial_write_str("\r\n");
        serial_write_str("BL: Framebuffer info ptr: 0x");
        serial_write_hex64(framebuffer_info_ptr);
        serial_write_str("\r\n");
        
        // Debug crítico: mostrar entry_reg exacto antes del salto
        serial_write_str("BL: VALOR FINAL entry_phys antes del ASM: 0x");
        serial_write_hex64(kernel_entry_phys);
        serial_write_str("\r\n");
        
        // Pasar el puntero a la estructura de framebuffer en rdi (primer argumento de la ABI x86_64)
        // Usar la dirección física calculada del entry point del kernel
        core::arch::asm!(
            "mov cr3, {cr3}",           // Configurar paginación
            "mov rsp, {rsp}",           // Configurar stack
            "sub rsp, 8",               // Alinear stack a 16 bytes
            
            "jmp rax",                  // Saltar al entry point (RAX)
            
            cr3 = in(reg) cr3_value,
            rsp = in(reg) rsp_alineado,
            in("rdi") framebuffer_info_ptr, // ARG1
            in("rsi") kernel_base,          // ARG2
            in("rax") kernel_entry_phys,    // Entry Point
        );

        // Si llegamos aquí, el kernel retornó (no debería pasar)
        serial_write_str("BL: ERROR - kernel retorno! Esto NO deberia pasar!\r\n");
        serial_write_str("BL: El kernel deberia haber tomado control completo\r\n");
    }
    
    // Si llegamos aquí, el kernel retornó (no debería pasar)
    Status::SUCCESS
}
