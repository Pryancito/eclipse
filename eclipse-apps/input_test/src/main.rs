//! Aplicación de test para el Sistema de Entrada Unificado
//! 
//! Esta aplicación demuestra cómo usar la API del InputSystem
//! para recibir eventos de teclado y ratón desde cualquier fuente.

#![cfg_attr(not(target_env = "gnu"), no_std)]
#![cfg_attr(not(target_env = "gnu"), no_main)]

extern crate alloc;

#[cfg(not(target_env = "gnu"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}

use eclipse_libc::{c_uint, println, exit};
use eclipse_ipc::prelude::*;

/// Punto de entrada para el entorno del host (Linux GNU)
#[cfg(target_env = "gnu")]
fn main() {
    init_app();
    test_input_system();
}

/// Punto de entrada de la aplicación en el target real
#[cfg(not(target_env = "gnu"))]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    init_app();
    test_input_system();
    exit_app(0);
}

/// Inicialización de la aplicación
fn init_app() {
    println!("INPUT_TEST: Iniciando aplicación de test...\n");
}

/// Test principal del InputSystem
fn test_input_system() {
    println!("INPUT_TEST: ========================================\n");
    println!("INPUT_TEST:   Test del Sistema de Entrada Unificado\n");
    println!("INPUT_TEST: ========================================\n");
    println!("INPUT_TEST: \n");
    println!("INPUT_TEST: Procesando eventos vía IpcChannel...\n");
    
    let mut ch = IpcChannel::new();
    
    let mut event_count = 0;
    let mut keyboard_count = 0;
    let mut mouse_count = 0;
    
    // Simular inyección de mensajes en el mock de libc
    inject_mock_input_events();
    
    // Loop principal - esperar y procesar hasta 10 mensajes (IPC asíncrono)
    for _ in 0..10 {
        let mut recv_fut = ch.recv_async();
        if let Some(msg) = block_on(&mut recv_fut) {
            event_count += 1;
            match msg {
                EclipseMessage::Input(ev) => {
                    println!("INPUT_TEST: [MSG] Recibido evento de entrada!");
                    
                    if ev.event_type == 0 {
                        keyboard_count += 1;
                        println!("INPUT_TEST:   - Tipo: TECLADO, Código: {}, Valor: {}\n", ev.code as c_uint, ev.value);
                    } else if ev.event_type == 1 {
                        mouse_count += 1;
                        println!("INPUT_TEST:   - Tipo: RATÓN (MOVE), Valor: {}\n", ev.value);
                    } else {
                        println!("INPUT_TEST:   - Tipo: OTRO ({}), Valor: {}\n", ev.event_type as c_uint, ev.value);
                    }
                }
                _ => {
                    println!("INPUT_TEST: [MSG] Recibido mensaje de otro tipo.\n");
                }
            }
        }
    }
    
    // Resumen final
    println!("INPUT_TEST: \n");
    println!("INPUT_TEST: ========================================\n");
    println!("INPUT_TEST:            Resumen Final\n");
    println!("INPUT_TEST: ========================================\n");
    println!("INPUT_TEST: Eventos procesados totales: {}\n", event_count);
    println!("INPUT_TEST: Pulsaciones teclado: {}\n", keyboard_count);
    println!("INPUT_TEST: Movimientos ratón: {}\n", mouse_count);
    println!("INPUT_TEST: ========================================\n");
    println!("INPUT_TEST: Test de entrada completado exitosamente!\n");
}

/// Inyectar eventos en el mock de libc para propósitos de test (HOST ONLY).
/// En el target real no se inyecta nada; en host sin mock del libc es no-op.
#[cfg(target_env = "gnu")]
fn inject_mock_input_events() {
    // Si eclipse_libc expone mock_push_receive (p. ej. feature "mock"), usarlo aquí.
    // Mientras tanto no-op: el test en host corre sin mensajes inyectados.
}

#[cfg(not(target_env = "gnu"))]
fn inject_mock_input_events() {
    // No-op en el target real
}

/// Salir de la aplicación
fn exit_app(code: i32) -> ! {
    println!("INPUT_TEST: Saliendo con código {}\n", code);
    unsafe { exit(code); }
}
