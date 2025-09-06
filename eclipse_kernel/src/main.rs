//! Punto de entrada principal del kernel Eclipse OS

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use eclipse_kernel::main_simple::kernel_main;

// El panic_handler está definido en la librería principal

#[no_mangle]
pub extern "C" fn _start() -> ! {
    kernel_main()
}
