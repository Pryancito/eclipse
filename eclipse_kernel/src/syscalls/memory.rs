//! Syscalls relacionadas con memoria
//! 
//! Este módulo implementa las syscalls para gestión de memoria, incluyendo
//! mapeo, desmapeo, protección y consejos de memoria.

use crate::debug::serial_write_str;
use super::{SyscallArgs, SyscallResult, SyscallError};

/// Gestor de memoria virtual
pub struct MemoryManager {
    /// Región de heap actual
    pub heap_start: u64,
    pub heap_end: u64,
    /// Regiones de memoria mapeadas
    pub mapped_regions: [Option<MemoryRegion>; 64],
}

/// Región de memoria mapeada
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub start: u64,
    pub end: u64,
    pub size: u64,
    pub prot: MemoryProtection,
    pub flags: MemoryFlags,
    pub fd: i32,
    pub offset: u64,
}

/// Protección de memoria
#[derive(Debug, Clone, Copy)]
pub struct MemoryProtection {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl MemoryProtection {
    /// Crear protección desde bits
    pub fn from_bits(bits: i32) -> Self {
        Self {
            read: (bits & 0x1) != 0,
            write: (bits & 0x2) != 0,
            execute: (bits & 0x4) != 0,
        }
    }

    /// Convertir a bits
    pub fn to_bits(&self) -> i32 {
        let mut bits = 0;
        if self.read { bits |= 0x1; }
        if self.write { bits |= 0x2; }
        if self.execute { bits |= 0x4; }
        bits
    }
}

/// Flags de memoria
#[derive(Debug, Clone, Copy)]
pub struct MemoryFlags {
    pub shared: bool,
    pub private: bool,
    pub anonymous: bool,
    pub fixed: bool,
    pub grow_down: bool,
    pub locked: bool,
    pub no_reserve: bool,
    pub populate: bool,
    pub non_blocking: bool,
    pub stack: bool,
    pub huge_tlb: bool,
}

impl MemoryFlags {
    /// Crear flags desde bits
    pub fn from_bits(bits: i32) -> Self {
        Self {
            shared: (bits & 0x01) != 0,
            private: (bits & 0x02) != 0,
            anonymous: (bits & 0x20) != 0,
            fixed: (bits & 0x10) != 0,
            grow_down: (bits & 0x00100000) != 0,
            locked: (bits & 0x20000000) != 0,
            no_reserve: (bits & 0x4000) != 0,
            populate: (bits & 0x00800000) != 0,
            non_blocking: (bits & 0x10000) != 0,
            stack: (bits & 0x20000) != 0,
            huge_tlb: (bits & 0x40000000) != 0,
        }
    }

    /// Convertir a bits
    pub fn to_bits(&self) -> i32 {
        let mut bits = 0;
        if self.shared { bits |= 0x01; }
        if self.private { bits |= 0x02; }
        if self.anonymous { bits |= 0x20; }
        if self.fixed { bits |= 0x10; }
        if self.grow_down { bits |= 0x00100000; }
        if self.locked { bits |= 0x20000000; }
        if self.no_reserve { bits |= 0x4000; }
        if self.populate { bits |= 0x00800000; }
        if self.non_blocking { bits |= 0x10000; }
        if self.stack { bits |= 0x20000; }
        if self.huge_tlb { bits |= 0x40000000; }
        bits
    }
}

impl MemoryManager {
    /// Crear nuevo gestor de memoria
    pub fn new() -> Self {
        Self {
            heap_start: 0x100000000, // 4GB
            heap_end: 0x100000000,   // 4GB
            mapped_regions: [None; 64],
        }
    }

    /// Mapear memoria
    pub fn mmap(&mut self, addr: u64, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> SyscallResult {
        serial_write_str(&alloc::format!("MEMORY_SYSCALL: mmap addr=0x{:x}, length={}, prot={}, flags={}\n", 
                                        addr, length, prot, flags));

        let protection = MemoryProtection::from_bits(prot);
        let mem_flags = MemoryFlags::from_bits(flags);

        // Buscar una región libre
        let mut new_addr = if addr == 0 {
            // Si no se especifica dirección, buscar una libre
            self.find_free_address(length)
        } else {
            addr
        };

        if new_addr == 0 {
            serial_write_str("MEMORY_SYSCALL: No se pudo encontrar dirección libre\n");
            return SyscallResult::Error(SyscallError::OutOfMemory);
        }

        // Crear nueva región
        let region = MemoryRegion {
            start: new_addr,
            end: new_addr + length as u64,
            size: length as u64,
            prot: protection,
            flags: mem_flags,
            fd,
            offset: offset as u64,
        };

        // Agregar a la tabla de regiones
        for i in 0..64 {
            if self.mapped_regions[i].is_none() {
                self.mapped_regions[i] = Some(region);
                serial_write_str(&alloc::format!("MEMORY_SYSCALL: Región mapeada en 0x{:x}\n", new_addr));
                return SyscallResult::Success(new_addr);
            }
        }

        serial_write_str("MEMORY_SYSCALL: Tabla de regiones llena\n");
        SyscallResult::Error(SyscallError::OutOfMemory)
    }

    /// Desmapear memoria
    pub fn munmap(&mut self, addr: u64, length: usize) -> SyscallResult {
        serial_write_str(&alloc::format!("MEMORY_SYSCALL: munmap addr=0x{:x}, length={}\n", addr, length));

        let end_addr = addr + length as u64;

        // Buscar y eliminar regiones que se superponen
        for i in 0..64 {
            if let Some(region) = self.mapped_regions[i] {
                if region.start < end_addr && region.end > addr {
                    self.mapped_regions[i] = None;
                    serial_write_str(&alloc::format!("MEMORY_SYSCALL: Región desmapeada: 0x{:x}-0x{:x}\n", 
                                                    region.start, region.end));
                }
            }
        }

        SyscallResult::Success(0)
    }

    /// Cambiar protección de memoria
    pub fn mprotect(&mut self, addr: u64, length: usize, prot: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("MEMORY_SYSCALL: mprotect addr=0x{:x}, length={}, prot={}\n", 
                                        addr, length, prot));

        let protection = MemoryProtection::from_bits(prot);
        let end_addr = addr + length as u64;

        // Buscar regiones que se superponen y cambiar su protección
        for i in 0..64 {
            if let Some(region) = &mut self.mapped_regions[i] {
                if region.start < end_addr && region.end > addr {
                    region.prot = protection;
                    serial_write_str(&alloc::format!("MEMORY_SYSCALL: Protección cambiada para región 0x{:x}-0x{:x}\n", 
                                                    region.start, region.end));
                }
            }
        }

        SyscallResult::Success(0)
    }

    /// Sincronizar memoria mapeada
    pub fn msync(&mut self, addr: u64, length: usize, flags: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("MEMORY_SYSCALL: msync addr=0x{:x}, length={}, flags={}\n", 
                                        addr, length, flags));

        // TODO: Implementar sincronización real con el disco
        // Por ahora solo logueamos
        serial_write_str("MEMORY_SYSCALL: Memoria sincronizada (simulado)\n");
        SyscallResult::Success(0)
    }

    /// Dar consejos sobre uso de memoria
    pub fn madvise(&mut self, addr: u64, length: usize, advice: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("MEMORY_SYSCALL: madvise addr=0x{:x}, length={}, advice={}\n", 
                                        addr, length, advice));

        match advice {
            1 => serial_write_str("MEMORY_SYSCALL: Consejo: MADV_NORMAL\n"),
            2 => serial_write_str("MEMORY_SYSCALL: Consejo: MADV_RANDOM\n"),
            3 => serial_write_str("MEMORY_SYSCALL: Consejo: MADV_SEQUENTIAL\n"),
            4 => serial_write_str("MEMORY_SYSCALL: Consejo: MADV_WILLNEED\n"),
            5 => serial_write_str("MEMORY_SYSCALL: Consejo: MADV_DONTNEED\n"),
            _ => serial_write_str(&alloc::format!("MEMORY_SYSCALL: Consejo desconocido: {}\n", advice)),
        }

        SyscallResult::Success(0)
    }

    /// Cambiar tamaño del heap
    pub fn brk(&mut self, addr: u64) -> SyscallResult {
        serial_write_str(&alloc::format!("MEMORY_SYSCALL: brk addr=0x{:x}\n", addr));

        if addr == 0 {
            // Retornar el break actual
            SyscallResult::Success(self.heap_end)
        } else {
            // Cambiar el break
            if addr >= self.heap_start {
                self.heap_end = addr;
                serial_write_str(&alloc::format!("MEMORY_SYSCALL: Heap extendido a 0x{:x}\n", addr));
                SyscallResult::Success(addr)
            } else {
                serial_write_str("MEMORY_SYSCALL: Dirección de break inválida\n");
                SyscallResult::Error(SyscallError::InvalidArgument)
            }
        }
    }

    /// Encontrar dirección libre para mapear
    fn find_free_address(&self, length: usize) -> u64 {
        let mut candidate = 0x200000000; // 8GB

        loop {
            let end = candidate + length as u64;
            let mut conflict = false;

            // Verificar conflictos con regiones existentes
            for region in &self.mapped_regions {
                if let Some(region) = region {
                    if !(end <= region.start || candidate >= region.end) {
                        conflict = true;
                        break;
                    }
                }
            }

            if !conflict {
                return candidate;
            }

            candidate += 0x1000; // Alinear a página de 4KB

            // Evitar desbordamiento
            if candidate > 0x7FFFFFFFFFFF {
                return 0;
            }
        }
    }

    /// Obtener información de debug
    pub fn debug_info(&self) {
        serial_write_str("MEMORY_SYSCALL: Información de memoria:\n");
        serial_write_str(&alloc::format!("MEMORY_SYSCALL: Heap: 0x{:x}-0x{:x}\n", 
                                        self.heap_start, self.heap_end));

        let mut count = 0;
        for region in &self.mapped_regions {
            if let Some(region) = region {
                count += 1;
                serial_write_str(&alloc::format!("MEMORY_SYSCALL: Región {}: 0x{:x}-0x{:x} ({} bytes)\n", 
                                                count, region.start, region.end, region.size));
            }
        }

        if count == 0 {
            serial_write_str("MEMORY_SYSCALL: No hay regiones mapeadas\n");
        }
    }
}

// Gestor global de memoria
static mut MEMORY_MANAGER: Option<MemoryManager> = None;

/// Inicializar el gestor de memoria
pub fn init_memory_manager() {
    unsafe {
        MEMORY_MANAGER = Some(MemoryManager::new());
        serial_write_str("MEMORY_SYSCALL: Gestor de memoria inicializado\n");
    }
}

/// Obtener referencia al gestor de memoria
pub fn get_memory_manager() -> &'static mut MemoryManager {
    unsafe {
        MEMORY_MANAGER.as_mut().expect("Gestor de memoria no inicializado")
    }
}

/// Syscall mmap implementada
pub fn sys_mmap_impl(addr: u64, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> SyscallResult {
    get_memory_manager().mmap(addr, length, prot, flags, fd, offset)
}

/// Syscall munmap implementada
pub fn sys_munmap_impl(addr: u64, length: usize) -> SyscallResult {
    get_memory_manager().munmap(addr, length)
}

/// Syscall mprotect implementada
pub fn sys_mprotect_impl(addr: u64, length: usize, prot: i32) -> SyscallResult {
    get_memory_manager().mprotect(addr, length, prot)
}

/// Syscall msync implementada
pub fn sys_msync_impl(addr: u64, length: usize, flags: i32) -> SyscallResult {
    get_memory_manager().msync(addr, length, flags)
}

/// Syscall madvise implementada
pub fn sys_madvise_impl(addr: u64, length: usize, advice: i32) -> SyscallResult {
    get_memory_manager().madvise(addr, length, advice)
}

/// Syscall brk implementada
pub fn sys_brk_impl(addr: u64) -> SyscallResult {
    get_memory_manager().brk(addr)
}

/// Obtener información de memoria para debug
pub fn debug_memory_info() {
    get_memory_manager().debug_info();
}

/// Pruebas de memoria
pub fn test_memory_syscalls() {
    serial_write_str("MEMORY_SYSCALL: Iniciando pruebas de syscalls de memoria\n");

    let manager = get_memory_manager();

    // Probar mmap
    let result = manager.mmap(0, 4096, 0x3, 0x22, -1, 0); // PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS
    serial_write_str(&alloc::format!("MEMORY_SYSCALL: mmap result: {:?}\n", result));

    // Probar mprotect
    let result = manager.mprotect(0x200000000, 4096, 0x1); // PROT_READ
    serial_write_str(&alloc::format!("MEMORY_SYSCALL: mprotect result: {:?}\n", result));

    // Probar brk
    let result = manager.brk(0x100000400); // Extender heap
    serial_write_str(&alloc::format!("MEMORY_SYSCALL: brk result: {:?}\n", result));

    // Mostrar información de debug
    manager.debug_info();

    serial_write_str("MEMORY_SYSCALL: Pruebas completadas\n");
}
