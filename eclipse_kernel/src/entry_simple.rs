#![no_std]
#![no_main]

// Usar el crate de librería para acceder a `main_simple`
extern crate eclipse_kernel;

// Allocator global mínimo (stub) para satisfacer `alloc` si algún módulo lo requiere
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

struct SimpleAllocator;

unsafe impl GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 { null_mut() }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator;

// Punto de entrada del kernel simplificado
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Delegar a la función principal del kernel en el crate de librería
    eclipse_kernel::main_simple::kernel_main();
}
