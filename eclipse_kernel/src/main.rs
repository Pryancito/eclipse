//! Punto de entrada principal del kernel Eclipse OS

#![no_std]
#![no_main]

// use core::panic::PanicInfo;
use core::error::Error;
extern crate alloc;
use alloc::boxed::Box;

// Importar funciones necesarias
use eclipse_kernel::main_simple::{serial_write_str, serial_init, kernel_main};

// Usamos el panic handler definido en lib.rs
// Punto de entrada principal del kernel (con parámetros del framebuffer)
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // ⚠️  KERNEL ULTRA-SIMPLE PARA DIAGNOSTICAR PAGE FAULT ⚠️
    // Solo las operaciones más básicas para identificar el problema

    unsafe {
        // 1. Inicializar serial (operaciones I/O directas, no deberían causar PF)
        serial_init();

        // 2. Mensaje simple sin acceder a memoria compleja
        serial_write_str("KERNEL: _start OK\r\n");

        unsafe {
            core::arch::asm!(
                "call {kernel_call}",
                kernel_call = sym kernel_call,
            );
        }
        
        // 3. Loop simple sin llamadas complejas
        loop {
            // Simular espera sin hlt
            for _ in 0..100000 {
                core::hint::spin_loop();
            }
        }
    }
}

unsafe fn kernel_call() -> Result<(), &'static str>{
    kernel_main()?;

    // Después de la inicialización, el kernel debe continuar ejecutándose
    // En Linux real, aquí entraría el scheduler principal
    kernel_main_loop()
}

unsafe fn kernel_main_loop() -> Result<(), &'static str> {
    serial_write_str("[KERNEL] Inicialización completada - Iniciando scheduler principal\r\n");
    serial_write_str("[KERNEL] Eclipse OS v0.5.0 ejecutándose\r\n");
    serial_write_str("[KERNEL] Presiona Ctrl+C para detener QEMU\r\n");

    loop {
        // En Linux real aquí iría:
        // - schedule() - scheduling de procesos
        // - Manejo de interrupciones
        // - Timer ticks
        // - etc.

        // TEMPORALMENTE DESHABILITADO: Instrucción hlt causa opcode inválido
        // Simular espera de interrupciones
        unsafe {
            serial_write_str("[KERNEL] Esperando interrupciones (hlt deshabilitado)\r\n");
        }
        for _ in 0..10000 {
            core::hint::spin_loop();
        }
    }
}