//! Heap allocator for eclipse-relibc
//! 
//! Optimización: Se utiliza un esquema de "Chunks" con un puntero de avance (bump pointer)
//! para evitar mmaps constantes, manteniendo la estabilidad del sistema.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use crate::eclipse_syscall::call::{mmap, munmap};
use crate::eclipse_syscall::flag::*;
pub use crate::types::*;
pub use crate::*;

// --- Constantes de Configuración ---
const ALIGNMENT: usize = 16;
const CHUNK_SIZE: usize = 2 * 1024 * 1024; // 2MB
const LARGE_THRESHOLD: usize = 1024 * 1024; // 1MB
const PAGE_SIZE: usize = 4096;

// --- Utilidades de Alineación ---
#[inline]
fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

#[inline]
fn round_up_page(size: usize) -> usize {
    align_up(size, PAGE_SIZE)
}

/// Estructura de control para un bloque de memoria
/// [SIZE (usize)][USER DATA...]
#[repr(C, align(16))]
struct BlockHeader {
    size: usize,
    _padding: usize, // Ensure header is 16 bytes so base+16 is 16-byte aligned
}

pub struct Allocator {
    // Para simplificar y evitar Page Faults por concurrencia en el freelist,
    // usaremos un esquema de asignación por chunks simple (Bump allocation).
    current_chunk: AtomicPtr<u8>,
    remaining: AtomicUsize,
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[cfg_attr(all(feature = "allocator", not(feature = "no-allocator")), global_allocator)]
static ALLOCATOR: Allocator = Allocator::new();

impl Allocator {
    pub const fn new() -> Self {
        Self {
            current_chunk: AtomicPtr::new(ptr::null_mut()),
            remaining: AtomicUsize::new(0),
        }
    }

    #[inline(never)]
    fn oom(&self, layout: Layout) -> ! {
        // En un entorno de sistema, un panic es mejor que un retorno nulo silencioso
        // si el resto del código no está preparado para Option<*mut u8>
        panic!("Out of Memory: size {} alignment {}", layout.size(), layout.align());
    }
}

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        if size == 0 { return ptr::null_mut(); }

        // Calculamos el tamaño total necesario incluyendo el header y alineación
        let header_size = core::mem::size_of::<BlockHeader>();
        let total_size = align_up(size + header_size, ALIGNMENT);

        // --- Caso 1: Asignación Grande (Directa a mmap) ---
        if total_size >= LARGE_THRESHOLD {
            let map_size = round_up_page(total_size);
            match eclipse_syscall::call::mmap(0, map_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) {
                Ok(addr) => {
                    let header = addr as *mut BlockHeader;
                    (*header).size = map_size; // Guardamos el tamaño total para munmap
                    return (addr as *mut u8).add(header_size);
                }
                Err(_) => self.oom(layout),
            }
        }

        // --- Caso 2: Asignación Pequeña (Uso de Chunks) ---
        // Intentamos obtener memoria del chunk actual de forma atómica
        loop {
            let curr_rem = self.remaining.load(Ordering::Acquire);
            let curr_ptr = self.current_chunk.load(Ordering::Acquire);

            if !curr_ptr.is_null() && curr_rem >= total_size {
                // Hay espacio: Intentamos "reservar" el espacio moviendo el puntero
                let next_ptr = curr_ptr.add(total_size);
                let next_rem = curr_rem - total_size;

                // CAS para asegurar que ningún otro hilo nos robó el espacio
                if self.current_chunk.compare_exchange(curr_ptr, next_ptr, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                    self.remaining.store(next_rem, Ordering::Release);
                    
                    let header = curr_ptr as *mut BlockHeader;
                    (*header).size = total_size;
                    return curr_ptr.add(header_size);
                }
                continue; // Reintentar si falló el CAS
            }

            // No hay espacio o no hay chunk: Mapear uno nuevo
            let map_size = round_up_page(CHUNK_SIZE);
            match eclipse_syscall::call::mmap(0, map_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) {
                Ok(addr) => {
                    // Establecemos el nuevo chunk. El remanente es map_size - lo que usamos ahora.
                    self.current_chunk.store((addr as *mut u8).add(total_size), Ordering::Release);
                    self.remaining.store(map_size - total_size, Ordering::Release);

                    let header = addr as *mut BlockHeader;
                    (*header).size = total_size;
                    return (addr as *mut u8).add(header_size);
                }
                Err(_) => self.oom(layout),
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() { return; }

        let header_ptr = ptr.sub(core::mem::size_of::<BlockHeader>()) as *mut BlockHeader;
        let size = (*header_ptr).size;

        // Si es una asignación grande, devolvemos al sistema inmediatamente
        if size >= LARGE_THRESHOLD {
            let _ = eclipse_syscall::call::munmap(header_ptr as usize, size);
        }
        
        // Si es pequeña, en este modelo de "Bump Allocation" no podemos devolver
        // fragmentos individuales al chunk sin una freelist compleja.
        // Se queda mapeado para evitar el overhead de mmaps/munmaps constantes.
    }
}

// --- Implementación de funciones C (malloc, free, etc.) ---
#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
mod imp {
    use super::*;

    #[no_mangle]
    pub unsafe extern "C" fn malloc(size: size_t) -> *mut c_void {
        let layout = Layout::from_size_align_unchecked(size as usize, ALIGNMENT);
        ALLOCATOR.alloc(layout) as *mut c_void
    }

    #[no_mangle]
    pub unsafe extern "C" fn free(ptr: *mut c_void) {
        if !ptr.is_null() {
            let layout = Layout::from_size_align_unchecked(0, ALIGNMENT);
            ALLOCATOR.dealloc(ptr as *mut u8, layout);
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn calloc(nmemb: size_t, size: size_t) -> *mut c_void {
        let total = nmemb.saturating_mul(size);
        let ptr = malloc(total);
        if !ptr.is_null() {
            ptr::write_bytes(ptr as *mut u8, 0, total as usize);
        }
        ptr
    }

    #[no_mangle]
    pub unsafe extern "C" fn realloc(ptr: *mut c_void, new_size: size_t) -> *mut c_void {
        if ptr.is_null() { return malloc(new_size); }
        if new_size == 0 { free(ptr); return ptr::null_mut(); }

        let header = (ptr as *mut u8).sub(core::mem::size_of::<BlockHeader>()) as *mut BlockHeader;
        let old_total_size = (*header).size;
        let old_user_size = old_total_size - core::mem::size_of::<BlockHeader>();

        if (new_size as usize) <= old_user_size {
            return ptr;
        }

        let new_ptr = malloc(new_size);
        if !new_ptr.is_null() {
            // Safely copy only the bounds of the old data to avoid Page Faults on resize.
            let copy_len = if (new_size as usize) < old_user_size { new_size as usize } else { old_user_size };
            ptr::copy_nonoverlapping(ptr as *const u8, new_ptr as *mut u8, copy_len);
            free(ptr);
        }
        new_ptr
    }
}

// Eclipse: usar nuestro allocator; host (tests / Linux sin eclipse_target): usar libc del sistema
#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
pub use imp::{malloc, free, calloc, realloc};

#[cfg(any(test, feature = "host-testing", all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(any(target_os = "eclipse", eclipse_target)))))]
mod imp {
    use super::*;

    extern "C" {
        #[link_name = "malloc"]
        fn sys_malloc(size: size_t) -> *mut c_void;
        #[link_name = "free"]
        fn sys_free(ptr: *mut c_void);
        #[link_name = "calloc"]
        fn sys_calloc(nmemb: size_t, size: size_t) -> *mut c_void;
        #[link_name = "realloc"]
        fn sys_realloc(ptr: *mut c_void, size: size_t) -> *mut c_void;
    }

    #[allow(dead_code)]
    pub unsafe fn malloc(size: size_t) -> *mut c_void {
        sys_malloc(size)
    }
    #[allow(dead_code)]
    pub unsafe fn free(ptr: *mut c_void) {
        sys_free(ptr)
    }
    #[allow(dead_code)]
    pub unsafe fn calloc(nmemb: size_t, size: size_t) -> *mut c_void {
        sys_calloc(nmemb, size)
    }
    #[allow(dead_code)]
    pub unsafe fn realloc(ptr: *mut c_void, size: size_t) -> *mut c_void {
        sys_realloc(ptr, size)
    }
}

#[cfg(any(test, feature = "host-testing", all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(any(target_os = "eclipse", eclipse_target)))))]
pub use imp::{malloc, free, calloc, realloc};
