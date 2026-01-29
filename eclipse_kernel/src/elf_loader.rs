//! Cargador de ejecutables ELF64 para Eclipse OS
//!
//! Este módulo maneja la carga y ejecución de binarios ELF64 en el userland

use alloc::vec::Vec;
use core::mem;
use core::ptr;
use spin::Mutex;
use core::sync::atomic::{AtomicBool, Ordering};

/// Sistema de ELF loader global
static ELF_LOADER_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Inicializar el cargador ELF
pub fn init_elf_loader() -> Result<(), &'static str> {
    if ELF_LOADER_INITIALIZED.load(Ordering::Acquire) {
        return Ok(());
    }
    
    // Inicializar estructuras globales del ELF loader si es necesario
    // Por ahora, el loader es stateless, así que solo marcamos como inicializado
    
    ELF_LOADER_INITIALIZED.store(true, Ordering::Release);
    Ok(())
}

/// Verificar si el ELF loader está inicializado
pub fn is_elf_loader_initialized() -> bool {
    ELF_LOADER_INITIALIZED.load(Ordering::Acquire)
}

/// Estructura del header ELF64
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Elf64Ehdr {
    pub e_ident: [u8; 16], // Identificación ELF
    pub e_type: u16,       // Tipo de archivo
    pub e_machine: u16,    // Arquitectura
    pub e_version: u32,    // Versión
    pub e_entry: u64,      // Punto de entrada
    pub e_phoff: u64,      // Offset de program headers
    pub e_shoff: u64,      // Offset de section headers
    pub e_flags: u32,      // Flags específicos de la máquina
    pub e_ehsize: u16,     // Tamaño del header
    pub e_phentsize: u16,  // Tamaño de program header
    pub e_phnum: u16,      // Número de program headers
    pub e_shentsize: u16,  // Tamaño de section header
    pub e_shnum: u16,      // Número de section headers
    pub e_shstrndx: u16,   // Índice de string table
}

/// Estructura del program header ELF64
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Elf64Phdr {
    pub p_type: u32,   // Tipo de segmento
    pub p_flags: u32,  // Flags del segmento
    pub p_offset: u64, // Offset en el archivo
    pub p_vaddr: u64,  // Dirección virtual
    pub p_paddr: u64,  // Dirección física
    pub p_filesz: u64, // Tamaño en el archivo
    pub p_memsz: u64,  // Tamaño en memoria
    pub p_align: u64,  // Alineación
}

/// Constantes ELF
const PT_LOAD: u32 = 1;
pub const PF_X: u32 = 1;  // Execute permission
pub const PF_W: u32 = 2;  // Write permission
pub const PF_R: u32 = 4;  // Read permission

/// Información de un segmento cargado
#[derive(Debug, Clone)]
pub struct LoadedSegment {
    pub vaddr: u64,
    pub size: u64,
    pub flags: u32,  // Flags del ELF (PF_R, PF_W, PF_X)
    pub physical_pages: Vec<u64>,  // Páginas físicas asignadas
}

/// Información del proceso cargado
#[derive(Debug, Clone)]
pub struct LoadedProcess {
    pub entry_point: u64,
    pub stack_pointer: u64,
    pub heap_start: u64,
    pub heap_end: u64,
    pub text_start: u64,
    pub text_end: u64,
    pub data_start: u64,
    pub data_end: u64,
    pub segments: Vec<LoadedSegment>,  // Segmentos cargados
}

/// Resultado de la carga de un proceso
pub type LoadResult = Result<LoadedProcess, &'static str>;

/// Cargador ELF64
pub struct ElfLoader {
    base_address: u64,
    next_address: u64,
    segments: Vec<LoadedSegment>,  // Segmentos cargados
}

impl ElfLoader {
    /// Crear nuevo cargador ELF
    pub fn new() -> Self {
        Self {
            base_address: 0x400000, // Dirección base para userland
            next_address: 0x400000,
            segments: Vec::new(),
        }
    }

    /// Cargar un ejecutable ELF64
    pub fn load_elf(&mut self, elf_data: &[u8]) -> LoadResult {
        // Verificar que tenemos suficientes datos para el header
        if elf_data.len() < mem::size_of::<Elf64Ehdr>() {
            return Err("Archivo ELF demasiado pequeño");
        }

        // Leer el header ELF
        let header = unsafe { ptr::read(elf_data.as_ptr() as *const Elf64Ehdr) };

        // Verificar la firma ELF
        if !self.verify_elf_signature(&header) {
            return Err("Archivo no es un ELF válido");
        }

        // Verificar que es un ejecutable
        if header.e_type != 2 {
            // ET_EXEC
            return Err("Archivo no es un ejecutable");
        }

        // Verificar arquitectura x86_64
        if header.e_machine != 62 {
            // EM_X86_64
            return Err("Archivo no es compatible con x86_64");
        }

        // Limpiar segmentos anteriores
        self.segments.clear();

        // Cargar segmentos
        self.load_segments(elf_data, &header)?;

        // Crear información del proceso
        let process = LoadedProcess {
            entry_point: header.e_entry,
            stack_pointer: self.setup_stack(),
            heap_start: self.next_address,
            heap_end: self.next_address + 0x100000, // 1MB de heap
            text_start: self.base_address,
            text_end: self.next_address,
            data_start: self.next_address,
            data_end: self.next_address,
            segments: self.segments.clone(),
        };

        Ok(process)
    }

    /// Verificar la firma ELF
    fn verify_elf_signature(&self, header: &Elf64Ehdr) -> bool {
        header.e_ident[0] == 0x7F &&
        header.e_ident[1] == b'E' &&
        header.e_ident[2] == b'L' &&
        header.e_ident[3] == b'F' &&
        header.e_ident[4] == 2 &&  // ELF64
        header.e_ident[5] == 1 &&  // Little endian
        header.e_ident[6] == 1 // Version 1
    }

    /// Cargar segmentos del archivo ELF
    fn load_segments(&mut self, elf_data: &[u8], header: &Elf64Ehdr) -> Result<(), &'static str> {
        let phdr_offset = header.e_phoff as usize;
        let phdr_size = header.e_phentsize as usize;
        let phdr_count = header.e_phnum as usize;

        // Verificar que tenemos suficientes datos para los program headers
        let required_size = phdr_offset + (phdr_count * phdr_size);
        if elf_data.len() < required_size {
            return Err("Datos insuficientes para program headers");
        }

        for i in 0..phdr_count {
            let phdr_ptr = unsafe { elf_data.as_ptr().add(phdr_offset + (i * phdr_size)) };
            let phdr = unsafe { ptr::read(phdr_ptr as *const Elf64Phdr) };

            // Solo cargar segmentos PT_LOAD
            if phdr.p_type == PT_LOAD {
                self.load_segment(elf_data, &phdr)?;
            }
        }

        Ok(())
    }

    /// Cargar un segmento individual
    fn load_segment(&mut self, elf_data: &[u8], phdr: &Elf64Phdr) -> Result<(), &'static str> {
        let file_offset = phdr.p_offset as usize;
        let file_size = phdr.p_filesz as usize;
        let mem_size = phdr.p_memsz as usize;
        let vaddr = phdr.p_vaddr;

        // Verificar que tenemos los datos del archivo
        if file_offset + file_size > elf_data.len() {
            return Err("Datos de segmento fuera del archivo");
        }

        // En un sistema real, aquí mapearíamos la memoria virtual
        // Por ahora, solo simulamos la carga
        self.simulate_memory_mapping(vaddr, mem_size as u64, phdr.p_flags)?;

        // Copiar datos del archivo a la memoria y obtener las páginas físicas asignadas
        // Esto incluye tanto segmentos con datos (code/data) como segmentos BSS (solo memoria)
        let physical_pages = if mem_size > 0 {
            self.copy_segment_data_with_pages(elf_data, file_offset, mem_size, vaddr)?
        } else {
            Vec::new()
        };

        // Guardar información del segmento
        self.segments.push(LoadedSegment {
            vaddr,
            size: mem_size as u64,
            flags: phdr.p_flags,
            physical_pages,
        });

        // Actualizar la siguiente dirección disponible
        self.next_address = vaddr + mem_size as u64;
        self.next_address = (self.next_address + 0x1000 - 1) & !0xFFF; // Alinear a página

        Ok(())
    }

    /// Simular mapeo de memoria
    fn simulate_memory_mapping(
        &self,
        vaddr: u64,
        size: u64,
        flags: u32,
    ) -> Result<(), &'static str> {
        // En un sistema real, aquí configuraríamos las tablas de páginas
        // y mapearíamos la memoria virtual

        // Verificar permisos
        let readable = (flags & PF_R) != 0;
        let writable = (flags & PF_W) != 0;
        let executable = (flags & PF_X) != 0;

        // Simular verificación de permisos
        if !readable {
            return Err("Segmento no es legible");
        }

        // Simular mapeo exitoso
        Ok(())
    }

    /// Copiar datos del segmento a páginas físicas (retorna las páginas asignadas)
    fn copy_segment_data_with_pages(
        &self,
        elf_data: &[u8],
        offset: usize,
        size: usize,
        vaddr: u64,
    ) -> Result<Vec<u64>, &'static str> {
        // Copiar los datos del ELF a páginas físicas asignadas
        
        if offset + size > elf_data.len() {
            return Err("Datos de segmento fuera de rango");
        }

        if size == 0 {
            return Ok(Vec::new());
        }

        // Alocar páginas físicas para este segmento
        let num_pages = (size + 4095) / 4096; // Redondear hacia arriba
        let mut allocated_pages = alloc::vec::Vec::new();
        
        for _ in 0..num_pages {
            if let Some(page_addr) = crate::memory::paging::allocate_physical_page() {
                allocated_pages.push(page_addr);
            } else {
                // Si falla la asignación, liberar las páginas ya asignadas
                for page in allocated_pages {
                    let _ = crate::memory::paging::deallocate_physical_page(page);
                }
                return Err("No hay suficiente memoria física para el segmento ELF");
            }
        }
        
        // Copiar los datos del ELF a las páginas físicas asignadas
        let mut bytes_processed = 0;
        for (page_idx, &page_addr) in allocated_pages.iter().enumerate() {
            let page_offset = page_idx * 4096;
            let bytes_in_page = core::cmp::min(4096, size - page_offset);
            
            if bytes_in_page == 0 {
                break;
            }
            
            // Copiar datos del archivo si están disponibles
            let file_bytes_in_page = if offset + page_offset < elf_data.len() {
                core::cmp::min(bytes_in_page, elf_data.len() - (offset + page_offset))
            } else {
                0
            };
            
            unsafe {
                let dst_ptr = page_addr as *mut u8;
                
                // Copiar datos del archivo
                if file_bytes_in_page > 0 {
                    let src_ptr = elf_data.as_ptr().add(offset + page_offset);
                    core::ptr::copy_nonoverlapping(src_ptr, dst_ptr, file_bytes_in_page);
                }
                
                // Llenar el resto con ceros (para BSS o padding)
                if file_bytes_in_page < 4096 {
                    core::ptr::write_bytes(
                        dst_ptr.add(file_bytes_in_page),
                        0,
                        4096 - file_bytes_in_page
                    );
                }
            }
            
            bytes_processed += bytes_in_page;
        }

        crate::debug::serial_write_str(&alloc::format!(
            "ELF_LOADER: Allocated {} physical pages and processed {} bytes for vaddr 0x{:x}\n",
            allocated_pages.len(), bytes_processed, vaddr
        ));

        Ok(allocated_pages)
    }

    /// Configurar la pila del proceso
    fn setup_stack(&mut self) -> u64 {
        // Reservar espacio para la pila (8MB)
        // Mantener la pila cerca del código para simplificar el mapeo de páginas
        // (según USERLAND_TRANSFER_FIX.md, usar 0x1000000 = 16MB)
        let stack_size = 0x800000; // 8MB
        let stack_end = 0x1000000; // 16MB (apunta al final de la pila)

        // La pila crece hacia abajo, así que el puntero inicia al final
        stack_end
    }
}

impl Default for ElfLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Función de utilidad para cargar eclipse-systemd
pub fn load_eclipse_systemd() -> LoadResult {
    // Intentar cargar desde el sistema de archivos virtual primero
    let elf_data = match load_systemd_from_vfs() {
        Ok(data) => {
            crate::debug::serial_write_str("ELF_LOADER: Loaded eclipse-systemd from VFS\n");
            data
        }
        Err(_) => {
            crate::debug::serial_write_str("ELF_LOADER: VFS not available, using fake ELF data\n");
            create_fake_elf_data()
        }
    };

    let mut loader = ElfLoader::new();
    loader.load_elf(&elf_data)
}

/// Cargar systemd desde el VFS
fn load_systemd_from_vfs() -> Result<Vec<u8>, &'static str> {
    use crate::vfs_global::get_vfs;
    
    let vfs = get_vfs();
    let mut vfs_lock = vfs.lock();
    
    // Intentar cargar /sbin/eclipse-systemd o /sbin/init
    let paths = ["/sbin/eclipse-systemd", "/sbin/init"];
    
    for path in &paths {
        match vfs_lock.read_file(path) {
            Ok(data) => {
                crate::debug::serial_write_str(&alloc::format!(
                    "ELF_LOADER: Loaded {} bytes from {}\n",
                    data.len(), path
                ));
                return Ok(data);
            }
            Err(_) => continue,
        }
    }
    
    Err("No se encontró systemd en VFS")
}

/// Crear datos ELF ficticios para simulación
fn create_fake_elf_data() -> Vec<u8> {
    let mut data = Vec::new();

    // Header ELF64 ficticio
    let header = Elf64Ehdr {
        e_ident: [0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        e_type: 2,     // ET_EXEC
        e_machine: 62, // EM_X86_64
        e_version: 1,
        e_entry: 0x400000,
        e_phoff: 64,
        e_shoff: 0,
        e_flags: 0,
        e_ehsize: 64,
        e_phentsize: 56,
        e_phnum: 2,
        e_shentsize: 0,
        e_shnum: 0,
        e_shstrndx: 0,
    };

    // Convertir header a bytes
    let header_bytes = unsafe {
        core::slice::from_raw_parts(
            &header as *const Elf64Ehdr as *const u8,
            mem::size_of::<Elf64Ehdr>(),
        )
    };
    data.extend_from_slice(header_bytes);

    // Program headers ficticios
    let text_phdr = Elf64Phdr {
        p_type: PT_LOAD,
        p_flags: PF_R | PF_X,
        p_offset: 0,
        p_vaddr: 0x400000,
        p_paddr: 0x400000,
        p_filesz: 0x1000,
        p_memsz: 0x1000,
        p_align: 0x1000,
    };

    let data_phdr = Elf64Phdr {
        p_type: PT_LOAD,
        p_flags: PF_R | PF_W,
        p_offset: 0x1000,
        p_vaddr: 0x401000,
        p_paddr: 0x401000,
        p_filesz: 0x1000,
        p_memsz: 0x1000,
        p_align: 0x1000,
    };

    // Convertir program headers a bytes
    let text_phdr_bytes = unsafe {
        core::slice::from_raw_parts(
            &text_phdr as *const Elf64Phdr as *const u8,
            mem::size_of::<Elf64Phdr>(),
        )
    };
    let data_phdr_bytes = unsafe {
        core::slice::from_raw_parts(
            &data_phdr as *const Elf64Phdr as *const u8,
            mem::size_of::<Elf64Phdr>(),
        )
    };

    data.extend_from_slice(text_phdr_bytes);
    data.extend_from_slice(data_phdr_bytes);

    // Rellenar con datos ficticios
    data.resize(0x2000, 0);

    data
}
