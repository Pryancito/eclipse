//! Sistema de memoria virtual para Eclipse OS
//! 
//! Implementa memoria virtual, swapping y protección de memoria

use alloc::vec::Vec;
use core::ops::Range;
use crate::paging::{PagingManager, PagePermission, PAGE_SIZE};

/// Región de memoria virtual
#[derive(Debug, Clone)]
pub struct VirtualMemoryRegion {
    pub start: usize,
    pub end: usize,
    pub permission: PagePermission,
    pub is_mapped: bool,
    pub is_swapped: bool,
    pub swap_offset: Option<usize>,
}

/// Configuración de memoria virtual
#[derive(Debug, Clone)]
pub struct VirtualMemoryConfig {
    pub virtual_address_space_size: usize,
    pub enable_swapping: bool,
    pub swap_file_size: usize,
    pub enable_memory_protection: bool,
    pub enable_address_space_randomization: bool,
}

impl Default for VirtualMemoryConfig {
    fn default() -> Self {
        Self {
            virtual_address_space_size: 4 * 1024 * 1024 * 1024, // 4GB
            enable_swapping: true,
            swap_file_size: 2 * 1024 * 1024 * 1024, // 2GB
            enable_memory_protection: true,
            enable_address_space_randomization: true,
        }
    }
}

/// Gestor de memoria virtual
pub struct VirtualMemoryManager {
    config: VirtualMemoryConfig,
    paging_manager: PagingManager,
    regions: Vec<VirtualMemoryRegion>,
    swap_space: Vec<u8>,
    next_virtual_address: usize,
    initialized: bool,
}

impl VirtualMemoryManager {
    pub fn new(config: VirtualMemoryConfig, total_physical_memory: usize) -> Self {
        Self {
            config,
            paging_manager: PagingManager::new(),
            regions: Vec::new(),
            swap_space: Vec::new(),
            next_virtual_address: 0x1000, // Empezar después de la página 0
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.initialized {
            return Err("Virtual memory manager already initialized");
        }

        // Inicializar el gestor de paginación
        self.paging_manager.initialize()?;

        // Inicializar espacio de swap si está habilitado
        if self.config.enable_swapping {
            self.swap_space = {
                let mut space = Vec::with_capacity(self.config.swap_file_size);
                for _ in 0..self.config.swap_file_size {
                    space.push(0);
                }
                space
            };
        }

        self.initialized = true;
        Ok(())
    }

    pub fn allocate_memory(&mut self, size: usize, permission: PagePermission) -> Result<usize, &'static str> {
        if !self.initialized {
            return Err("Virtual memory manager not initialized");
        }

        let page_count = (size + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize;
        let aligned_size = page_count * PAGE_SIZE as usize;

        // Buscar una región libre
        let virtual_start = self.find_free_region(aligned_size)?;

        // Asignar páginas físicas
        let physical_pages = self.paging_manager.allocate_pages(page_count)?;

        // Mapear memoria virtual a física
        for (i, &physical_addr) in physical_pages.iter().enumerate() {
            let virtual_addr = virtual_start + (i * PAGE_SIZE as usize);
            self.paging_manager.map_memory(
                virtual_addr,
                physical_addr,
                PAGE_SIZE as usize,
                permission,
            )?;
        }

        // Registrar la región
        let region = VirtualMemoryRegion {
            start: virtual_start,
            end: virtual_start + aligned_size,
            permission,
            is_mapped: true,
            is_swapped: false,
            swap_offset: None,
        };
        self.regions.push(region);

        Ok(virtual_start)
    }

    pub fn deallocate_memory(&mut self, virtual_addr: usize) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Virtual memory manager not initialized");
        }

        // Buscar la región
        if let Some(pos) = self.regions.iter().position(|r| r.start == virtual_addr) {
            let region = &self.regions[pos];
            
            // Desmapear la memoria
            self.paging_manager.unmap_memory(region.start)?;
            
            // Liberar páginas físicas
            let page_count = (region.end - region.start) / PAGE_SIZE as usize;
            let virtual_addresses: Vec<usize> = (0..page_count)
                .map(|i| region.start + (i * PAGE_SIZE as usize))
                .collect();
            self.paging_manager.deallocate_pages(&virtual_addresses)?;

            // Remover la región
            self.regions.remove(pos);
            Ok(())
        } else {
            Err("Memory region not found")
        }
    }

    pub fn map_memory(&mut self, virtual_addr: usize, physical_addr: usize, size: usize, permission: PagePermission) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Virtual memory manager not initialized");
        }

        let page_count = (size + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize;
        let aligned_size = page_count * PAGE_SIZE as usize;

        // Mapear memoria
        self.paging_manager.map_memory(
            virtual_addr,
            physical_addr,
            aligned_size,
            permission,
        )?;

        // Registrar la región
        let region = VirtualMemoryRegion {
            start: virtual_addr,
            end: virtual_addr + aligned_size,
            permission,
            is_mapped: true,
            is_swapped: false,
            swap_offset: None,
        };
        self.regions.push(region);

        Ok(())
    }

    pub fn unmap_memory(&mut self, virtual_addr: usize) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Virtual memory manager not initialized");
        }

        // Buscar la región
        if let Some(pos) = self.regions.iter().position(|r| r.start == virtual_addr) {
            let region = &self.regions[pos];
            
            // Desmapear la memoria
            self.paging_manager.unmap_memory(region.start)?;
            
            // Remover la región
            self.regions.remove(pos);
            Ok(())
        } else {
            Err("Memory region not found")
        }
    }

    pub fn translate_address(&self, virtual_addr: usize) -> Option<usize> {
        self.paging_manager.translate_address(virtual_addr).ok()
    }

    pub fn swap_out(&mut self, virtual_addr: usize) -> Result<(), &'static str> {
        if !self.config.enable_swapping {
            return Err("Swapping not enabled");
        }

        // Buscar la región
        if let Some(region_index) = self.regions.iter().position(|r| r.start == virtual_addr) {
            if self.regions[region_index].is_swapped {
                return Ok(()); // Ya está en swap
            }

            // Encontrar espacio en swap
            let region_size = self.regions[region_index].end - self.regions[region_index].start;
            let swap_offset = self.find_swap_space(region_size)?;
            
            // Copiar datos a swap (simulado)
            self.regions[region_index].is_swapped = true;
            self.regions[region_index].is_mapped = false;
            self.regions[region_index].swap_offset = Some(swap_offset);

            // Desmapear la memoria física
            self.paging_manager.unmap_memory(self.regions[region_index].start)?;

            Ok(())
        } else {
            Err("Memory region not found")
        }
    }

    pub fn swap_in(&mut self, virtual_addr: usize) -> Result<(), &'static str> {
        if !self.config.enable_swapping {
            return Err("Swapping not enabled");
        }

        // Buscar la región
        if let Some(region) = self.regions.iter_mut().find(|r| r.start == virtual_addr) {
            if !region.is_swapped {
                return Ok(()); // No está en swap
            }

            // Asignar nueva memoria física
            let page_count = (region.end - region.start) / PAGE_SIZE as usize;
            let physical_pages = self.paging_manager.allocate_pages(page_count)?;

            // Mapear memoria
            for (i, &physical_addr) in physical_pages.iter().enumerate() {
                let virtual_addr = region.start + (i * PAGE_SIZE as usize);
                self.paging_manager.map_memory(
                    virtual_addr,
                    physical_addr,
                    PAGE_SIZE as usize,
                    region.permission,
                )?;
            }

            // Restaurar desde swap (simulado)
            region.is_swapped = false;
            region.is_mapped = true;
            region.swap_offset = None;

            Ok(())
        } else {
            Err("Memory region not found")
        }
    }

    fn find_free_region(&mut self, size: usize) -> Result<usize, &'static str> {
        // Ordenar regiones por dirección
        self.regions.sort_by_key(|r| r.start);

        // Buscar espacio entre regiones existentes
        let mut last_end = 0x1000; // Empezar después de la página 0
        
        for region in &self.regions {
            if region.start - last_end >= size {
                return Ok(last_end);
            }
            last_end = region.end;
        }

        // Si no hay espacio, usar la siguiente dirección disponible
        let start = self.next_virtual_address;
        self.next_virtual_address += size;
        
        if self.next_virtual_address > self.config.virtual_address_space_size {
            return Err("Virtual address space exhausted");
        }

        Ok(start)
    }

    fn find_swap_space(&self, size: usize) -> Result<usize, &'static str> {
        // Implementación simplificada de búsqueda de espacio en swap
        // En un sistema real, esto sería más complejo
        Ok(0) // Simulado
    }

    pub fn get_memory_stats(&self) -> (usize, usize, usize, usize) {
        let stats = self.paging_manager.get_memory_stats();
        let swapped_regions = self.regions.iter().filter(|r| r.is_swapped).count();
        (stats.allocated_pages, stats.free_pages, stats.total_pages, swapped_regions)
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}
