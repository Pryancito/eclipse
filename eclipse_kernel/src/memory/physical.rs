//! Gestión de memoria física y traducción de direcciones
//!
//! Este módulo proporciona funciones para:
//! - Traducir direcciones virtuales a físicas
//! - Alocar memoria físicamente contigua
//! - Gestionar regiones DMA

// use crate::memory::manager::MEMORY_MANAGER;

/// Traduce una dirección virtual a su dirección física correspondiente
/// 
/// # Parámetros
/// - `virt_addr`: Dirección virtual a traducir
/// 
/// # Retorna
/// - `Some(phys_addr)`: La dirección física correspondiente
/// - `None`: Si la dirección no está mapeada
pub fn virt_to_phys(virt_addr: u64) -> Option<u64> {
    // En Eclipse OS, actualmente usamos identity mapping para el kernel
    // (direcciones virtuales = direcciones físicas en el espacio del kernel)
    
    // TODO: Implementar traducción real usando page tables
    // Por ahora, asumimos identity mapping para direcciones del kernel
    
    // Verificar que esté en rango de kernel (típicamente < 4GB para nuestro mapeo actual)
    if virt_addr < 0x100000000 {
        // Identity mapping
        Some(virt_addr)
    } else {
        // Fuera de rango mapeado
        None
    }
}

/// Traduce una dirección física a virtual (operación inversa)
///
/// # Parámetros
/// - `phys_addr`: Dirección física
///
/// # Retorna
/// - `Some(virt_addr)`: Dirección virtual correspondiente
/// - `None`: Si no hay mapeo
pub fn phys_to_virt(phys_addr: u64) -> Option<u64> {
    // Con identity mapping, es la misma dirección
    if phys_addr < 0x100000000 {
        Some(phys_addr)
    } else {
        None
    }
}

/// Aloca un bloque de memoria físicamente contigua
/// 
/// IMPORTANTE: La memoria alocada con esta función debe ser liberada
/// manualmente y NO será gestionada por el heap normal.
/// 
/// # Parámetros
/// - `size`: Tamaño en bytes (será redondeado a múltiplo de página)
/// - `alignment`: Alineación requerida en bytes
/// 
/// # Retorna
/// - `Some((virt_addr, phys_addr))`: Direcciones virtual y física del bloque
/// - `None`: Si no se pudo alocar
pub fn allocate_physically_contiguous(size: usize, alignment: usize) -> Option<(u64, u64)> {
    // TODO: Implementar alocación real de memoria física contigua
    // Por ahora, usamos el heap del kernel que ya es físicamente contiguo
    // debido a identity mapping
    
    use alloc::alloc::{alloc, Layout};
    
    // Crear layout con la alineación solicitada
    let layout = Layout::from_size_align(size, alignment).ok()?;
    
    unsafe {
        let ptr = alloc(layout);
        if ptr.is_null() {
            return None;
        }
        
        let virt_addr = ptr as u64;
        let phys_addr = virt_to_phys(virt_addr)?;
        
        Some((virt_addr, phys_addr))
    }
}

/// Libera memoria alocada con `allocate_physically_contiguous`
///
/// # Parámetros
/// - `virt_addr`: Dirección virtual retornada por `allocate_physically_contiguous`
/// - `size`: Tamaño original
/// - `alignment`: Alineación original
pub unsafe fn free_physically_contiguous(virt_addr: u64, size: usize, alignment: usize) {
    use alloc::alloc::{dealloc, Layout};
    
    if let Ok(layout) = Layout::from_size_align(size, alignment) {
        dealloc(virt_addr as *mut u8, layout);
    }
}

/// Verifica si una región de memoria es físicamente contigua
///
/// # Parámetros
/// - `virt_addr`: Dirección virtual de inicio
/// - `size`: Tamaño de la región
///
/// # Retorna
/// - `true`: La región es físicamente contigua
/// - `false`: La región NO es físicamente contigua
pub fn is_physically_contiguous(virt_addr: u64, size: usize) -> bool {
    // TODO: Implementar verificación real consultando page tables
    // Por ahora, con identity mapping todo es contiguo
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_virt_to_phys_identity() {
        // Con identity mapping, debería retornar la misma dirección
        assert_eq!(virt_to_phys(0x1000), Some(0x1000));
        assert_eq!(virt_to_phys(0x200000), Some(0x200000));
    }
    
    #[test]
    fn test_allocate_contiguous() {
        // Intentar alocar 4KB alineado a 64 bytes
        if let Some((virt, phys)) = allocate_physically_contiguous(4096, 64) {
            assert!(virt % 64 == 0); // Verificar alineación
            assert!(phys % 64 == 0);
            
            // Liberar
            unsafe {
                free_physically_contiguous(virt, 4096, 64);
            }
        }
    }
}

