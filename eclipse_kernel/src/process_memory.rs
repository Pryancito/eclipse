//! Gestión de memoria para procesos en Eclipse OS
//!
//! Este módulo maneja la asignación y gestión de memoria para procesos del userland

use core::mem;
use core::ptr;

/// Información de memoria de un proceso
#[derive(Debug, Clone)]
pub struct ProcessMemory {
    pub text_start: u64,
    pub text_end: u64,
    pub data_start: u64,
    pub data_end: u64,
    pub heap_start: u64,
    pub heap_end: u64,
    pub stack_start: u64,
    pub stack_end: u64,
    pub stack_pointer: u64, // Puntero de pila actual
    pub brk: u64,           // Break pointer para heap
}

/// Gestor de memoria de procesos
pub struct ProcessMemoryManager {
    next_vaddr: u64,
    page_size: u64,
}

impl ProcessMemoryManager {
    /// Crear nuevo gestor de memoria
    pub fn new() -> Self {
        Self {
            next_vaddr: 0x400000, // Dirección base para userland
            page_size: 0x1000,    // 4KB por página
        }
    }

    /// Asignar memoria para un proceso
    /// 
    /// # Errores
    /// 
    /// Retorna un error si:
    /// - Las direcciones calculadas no están alineadas a página (esto indica un error en la lógica)
    /// - El tamaño solicitado causaría desbordamiento de dirección
    pub fn allocate_process_memory(&mut self, text_size: u64, data_size: u64) -> Result<ProcessMemory, &'static str> {
        // Asegurar que next_vaddr esté alineado a página
        self.next_vaddr = self.align_to_page(self.next_vaddr);

        let text_start = self.next_vaddr;
        let text_end = text_start.checked_add(self.align_to_page(text_size))
            .ok_or("Desbordamiento al calcular text_end")?;

        let data_start = text_end;
        let data_end = data_start.checked_add(self.align_to_page(data_size))
            .ok_or("Desbordamiento al calcular data_end")?;

        let heap_start = data_end;
        let heap_end = heap_start.checked_add(0x100000) // 1MB de heap inicial
            .ok_or("Desbordamiento al calcular heap_end")?;

        let stack_size = 0x800000; // 8MB de stack
        // Usar una dirección de stack más baja que esté en el mismo rango mapeado
        // que el código (0x400000). Esto evita necesidad de múltiples estructuras de paginación.
        // Stack en 0x800000 - 0x1000000 (8MB-16MB rango)
        let stack_end: u64 = 0x1000000; // 16MB
        let stack_start = stack_end.checked_sub(stack_size)
            .ok_or("Desbordamiento al calcular stack_start")?;

        // Validar que las direcciones no estén mal alineadas (esto indica un bug en align_to_page)
        // En desarrollo, los debug_assert_eq! captarán estos errores.
        // En producción, verificamos por seguridad adicional.
        #[cfg(debug_assertions)]
        {
            debug_assert_eq!(text_start % self.page_size, 0, "text_start no alineado: 0x{:x}", text_start);
            debug_assert_eq!(text_end % self.page_size, 0, "text_end no alineado: 0x{:x}", text_end);
            debug_assert_eq!(data_start % self.page_size, 0, "data_start no alineado: 0x{:x}", data_start);
            debug_assert_eq!(data_end % self.page_size, 0, "data_end no alineado: 0x{:x}", data_end);
            debug_assert_eq!(heap_start % self.page_size, 0, "heap_start no alineado: 0x{:x}", heap_start);
            debug_assert_eq!(heap_end % self.page_size, 0, "heap_end no alineado: 0x{:x}", heap_end);
            debug_assert_eq!(stack_start % self.page_size, 0, "stack_start no alineado: 0x{:x}", stack_start);
            debug_assert_eq!(stack_end % self.page_size, 0, "stack_end no alineado: 0x{:x}", stack_end);
        }
        
        #[cfg(not(debug_assertions))]
        {
            if text_start % self.page_size != 0 || text_end % self.page_size != 0 ||
               data_start % self.page_size != 0 || data_end % self.page_size != 0 ||
               heap_start % self.page_size != 0 || heap_end % self.page_size != 0 ||
               stack_start % self.page_size != 0 || stack_end % self.page_size != 0 {
                return Err("Error interno: dirección no alineada a página");
            }
        }

        self.next_vaddr = heap_end;

        Ok(ProcessMemory {
            text_start,
            text_end,
            data_start,
            data_end,
            heap_start,
            heap_end,
            stack_start,
            stack_end,
            stack_pointer: stack_end, // Stack pointer apunta al final de la pila
            brk: heap_start,
        })
    }

    /// Mapear memoria virtual
    pub fn map_memory(
        &self,
        vaddr: u64,
        size: u64,
        flags: MemoryFlags,
    ) -> Result<(), &'static str> {
        // En un sistema real, aquí configuraríamos las tablas de páginas
        // Por ahora, solo simulamos el mapeo

        if !self.is_valid_address(vaddr) {
            return Err("Dirección virtual inválida");
        }

        if size == 0 {
            return Err("Tamaño de memoria inválido");
        }

        // Simular mapeo exitoso
        self.simulate_page_table_setup(vaddr, size, flags)?;

        Ok(())
    }

    /// Desmapear memoria virtual
    pub fn unmap_memory(&self, vaddr: u64, size: u64) -> Result<(), &'static str> {
        // En un sistema real, aquí limpiaríamos las entradas de la tabla de páginas
        // Por ahora, solo simulamos el desmapeo

        if !self.is_valid_address(vaddr) {
            return Err("Dirección virtual inválida");
        }

        // Simular desmapeo exitoso
        Ok(())
    }

    /// Verificar si una dirección es válida
    fn is_valid_address(&self, vaddr: u64) -> bool {
        // Verificar que esté en el espacio de direcciones del userland
        vaddr >= 0x400000 && vaddr < 0x7FFFFFFFFFFF
    }

    /// Simular configuración de tabla de páginas
    fn simulate_page_table_setup(
        &self,
        vaddr: u64,
        size: u64,
        flags: MemoryFlags,
    ) -> Result<(), &'static str> {
        // En un sistema real, aquí configuraríamos las entradas de la tabla de páginas
        // con los permisos apropiados

        // Verificar que vaddr esté alineado a página
        if vaddr % self.page_size != 0 {
            return Err("Dirección base no alineada a página");
        }

        let page_count = (size + self.page_size - 1) / self.page_size;

        for i in 0..page_count {
            // Usar checked_mul para prevenir desbordamiento en cálculo de offset
            let offset = i.checked_mul(self.page_size)
                .ok_or("Desbordamiento al calcular offset de página")?;
            
            let page_vaddr = vaddr.checked_add(offset)
                .ok_or("Desbordamiento al calcular dirección de página")?;

            // Simular configuración de página
            self.setup_page_entry(page_vaddr, flags)?;
        }

        Ok(())
    }

    /// Configurar entrada de página
    fn setup_page_entry(&self, vaddr: u64, flags: MemoryFlags) -> Result<(), &'static str> {
        // Verificar alineación de página
        if vaddr % self.page_size != 0 {
            return Err("Dirección no alineada a página");
        }

        // Configuración real de tabla de páginas
        // 1. Calcular índices de la tabla de páginas
        let pml4_index = (vaddr >> 39) & 0x1FF;
        let pdpt_index = (vaddr >> 30) & 0x1FF;
        let pd_index = (vaddr >> 21) & 0x1FF;
        let pt_index = (vaddr >> 12) & 0x1FF;

        // 2. Verificar que los índices sean válidos
        if pml4_index >= 512 || pdpt_index >= 512 || pd_index >= 512 || pt_index >= 512 {
            return Err("Índice de tabla de páginas inválido");
        }

        // 3. Configurar entrada de página con flags reales
        let page_flags = self.convert_memory_flags_to_page_flags(flags);

        // 4. En un sistema real, aquí escribiríamos en las tablas de páginas
        // Por ahora, solo verificamos que la configuración sea válida
        if page_flags == 0 {
            return Err("Flags de página inválidos");
        }

        // 5. Simular escritura exitosa en la tabla de páginas
        // En un sistema real, esto sería:
        // - Cargar la tabla PML4
        // - Verificar/crear entrada PDPT
        // - Verificar/crear entrada PD
        // - Verificar/crear entrada PT
        // - Escribir entrada de página con flags

        Ok(())
    }

    /// Convertir MemoryFlags a flags de página del sistema
    fn convert_memory_flags_to_page_flags(&self, flags: MemoryFlags) -> u64 {
        let mut page_flags = 0x01; // PRESENT bit

        if flags.contains(MemoryFlags::WRITE) {
            page_flags |= 0x02; // WRITABLE bit
        }

        if flags.contains(MemoryFlags::EXECUTE) {
            page_flags |= 0x04; // EXECUTE bit
        }

        if flags.contains(MemoryFlags::USER) {
            page_flags |= 0x08; // USER bit
        }

        page_flags
    }

    /// Alinear tamaño a múltiplo de página
    /// 
    /// # Errores
    /// 
    /// Retorna el tamaño alineado o satura a u64::MAX si hay overflow.
    /// En práctica, el overflow no debería ocurrir ya que page_size es pequeño (0x1000).
    fn align_to_page(&self, size: u64) -> u64 {
        // Usar saturating_add para prevenir overflow, aunque es improbable con page_size típico
        let aligned = size.saturating_add(self.page_size).saturating_sub(1);
        aligned & !(self.page_size - 1)
    }

    /// Expandir heap del proceso
    pub fn expand_heap(
        &self,
        process_mem: &mut ProcessMemory,
        size: u64,
    ) -> Result<u64, &'static str> {
        let aligned_size = self.align_to_page(size);
        let new_brk = process_mem.brk.checked_add(aligned_size)
            .ok_or("Desbordamiento al expandir heap")?;

        if new_brk > process_mem.heap_end {
            return Err("Heap excede límite máximo");
        }

        // Mapear nueva memoria del heap
        let heap_size = new_brk - process_mem.brk;
        self.map_memory(
            process_mem.brk,
            heap_size,
            MemoryFlags::READ | MemoryFlags::WRITE,
        )?;

        let old_brk = process_mem.brk;
        process_mem.brk = new_brk;

        Ok(old_brk)
    }

    /// Configurar argumentos del proceso
    /// 
    /// # Errores
    /// 
    /// Retorna un error si:
    /// - Los argumentos o variables de entorno causarían underflow del stack pointer
    /// - El resultado final no está alineado correctamente
    pub fn setup_process_args(
        &self,
        stack_ptr: u64,
        args: &[&str],
        env: &[&str],
    ) -> Result<u64, &'static str> {
        // En un sistema real, aquí colocaríamos los argumentos y variables de entorno
        // en la pila del proceso siguiendo la convención de llamada del sistema

        let mut current_ptr = stack_ptr;

        // Simular colocación de argumentos con verificación de underflow
        for arg in args {
            let arg_size = (arg.len() as u64).checked_add(1) // +1 para null terminator
                .ok_or("Argumento demasiado largo")?;
            
            current_ptr = current_ptr.checked_sub(arg_size)
                .ok_or("Stack underflow al colocar argumentos")?;
        }

        // Simular colocación de variables de entorno con verificación de underflow
        for env_var in env {
            let env_size = (env_var.len() as u64).checked_add(1) // +1 para null terminator
                .ok_or("Variable de entorno demasiado larga")?;
            
            current_ptr = current_ptr.checked_sub(env_size)
                .ok_or("Stack underflow al colocar variables de entorno")?;
        }

        // Alinear a 16 bytes (requisito de ABI x86_64)
        current_ptr = current_ptr & !0xF;

        Ok(current_ptr)
    }
}

impl Default for ProcessMemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Flags de memoria
#[derive(Debug, Clone, Copy)]
pub struct MemoryFlags(u32);

impl MemoryFlags {
    pub const READ: Self = Self(0x1);
    pub const WRITE: Self = Self(0x2);
    pub const EXECUTE: Self = Self(0x4);
    pub const USER: Self = Self(0x8);

    pub fn new() -> Self {
        Self(0)
    }

    pub fn contains(&self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn with_read(mut self) -> Self {
        self.0 |= 0x1;
        self
    }

    pub fn with_write(mut self) -> Self {
        self.0 |= 0x2;
        self
    }

    pub fn with_execute(mut self) -> Self {
        self.0 |= 0x4;
        self
    }

    pub fn with_user(mut self) -> Self {
        self.0 |= 0x8;
        self
    }
}

impl core::ops::BitOr for MemoryFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

/// Función de utilidad para configurar memoria de eclipse-systemd
pub fn setup_eclipse_systemd_memory() -> Result<ProcessMemory, &'static str> {
    let mut manager = ProcessMemoryManager::new();

    // Asignar memoria para el proceso
    let process_mem = manager.allocate_process_memory(0x1000, 0x1000)?;

    // Mapear segmento de texto (ejecutable)
    manager.map_memory(
        process_mem.text_start,
        process_mem.text_end - process_mem.text_start,
        MemoryFlags::READ | MemoryFlags::EXECUTE | MemoryFlags::USER,
    )?;

    // Mapear segmento de datos
    manager.map_memory(
        process_mem.data_start,
        process_mem.data_end - process_mem.data_start,
        MemoryFlags::READ | MemoryFlags::WRITE | MemoryFlags::USER,
    )?;

    // Mapear heap
    manager.map_memory(
        process_mem.heap_start,
        process_mem.heap_end - process_mem.heap_start,
        MemoryFlags::READ | MemoryFlags::WRITE | MemoryFlags::USER,
    )?;

    // Mapear stack
    manager.map_memory(
        process_mem.stack_start,
        process_mem.stack_end - process_mem.stack_start,
        MemoryFlags::READ | MemoryFlags::WRITE | MemoryFlags::USER,
    )?;

    Ok(process_mem)
}
