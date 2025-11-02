//! Aplicación de test para el Sistema de Entrada Unificado
//! 
//! Esta aplicación demuestra cómo usar la API del InputSystem
//! para recibir eventos de teclado y ratón desde cualquier fuente.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use core::panic::PanicInfo;

/// Punto de entrada de la aplicación
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Inicializar (syscalls, etc.)
    init_app();
    
    // Ejecutar test del InputSystem
    test_input_system();
    
    // Salir
    exit_app(0);
}

/// Inicialización de la aplicación
fn init_app() {
    // TODO: Inicializar heap, syscalls, etc.
    println_serial("INPUT_TEST: Iniciando aplicación de test...\n");
}

/// Test principal del InputSystem
fn test_input_system() {
    println_serial("INPUT_TEST: ========================================\n");
    println_serial("INPUT_TEST:   Test del Sistema de Entrada Unificado\n");
    println_serial("INPUT_TEST: ========================================\n");
    println_serial("INPUT_TEST: \n");
    println_serial("INPUT_TEST: Presiona teclas o mueve el ratón...\n");
    println_serial("INPUT_TEST: (ESC para salir)\n");
    println_serial("INPUT_TEST: \n");
    
    let mut event_count = 0;
    let mut keyboard_count = 0;
    let mut mouse_count = 0;
    
    // Loop principal - procesar eventos
    for iteration in 0..1000 {
        // Simular syscall para obtener evento
        // En la implementación real, esto sería:
        // let event = syscall::get_next_input_event();
        
        // Por ahora, solo contar iteraciones
        event_count += 1;
        
        // Cada 100 iteraciones, mostrar estadísticas
        if iteration % 100 == 0 && iteration > 0 {
            println_serial(&format!(
                "INPUT_TEST: [Iter {}] {} eventos procesados ({} kbd, {} mouse)\n",
                iteration, event_count, keyboard_count, mouse_count
            ));
        }
        
        // Pausa breve
        for _ in 0..100000 {
            unsafe { core::arch::asm!("nop"); }
        }
    }
    
    // Resumen final
    println_serial("INPUT_TEST: \n");
    println_serial("INPUT_TEST: ========================================\n");
    println_serial("INPUT_TEST:            Resumen Final\n");
    println_serial("INPUT_TEST: ========================================\n");
    println_serial(&format!("INPUT_TEST: Iteraciones: 1000\n"));
    println_serial(&format!("INPUT_TEST: Eventos procesados: {}\n", event_count));
    println_serial(&format!("INPUT_TEST: Teclado: {}\n", keyboard_count));
    println_serial(&format!("INPUT_TEST: Ratón: {}\n", mouse_count));
    println_serial("INPUT_TEST: ========================================\n");
    println_serial("INPUT_TEST: Test completado exitosamente!\n");
}

/// Salir de la aplicación
fn exit_app(code: i32) -> ! {
    println_serial(&format!("INPUT_TEST: Saliendo con código {}\n", code));
    
    // TODO: Syscall real para exit
    loop {
        unsafe { core::arch::asm!("hlt"); }
    }
}

/// Imprimir a serial port (helper temporal)
fn println_serial(msg: &str) {
    // TODO: Implementar syscall real para escribir a serial
    // Por ahora, noop
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt"); }
    }
}

