//! Aplicación de test para el Sistema de Entrada Unificado
//! 
//! Esta aplicación demuestra cómo usar la API del InputSystem
//! para recibir eventos de teclado y ratón desde cualquier fuente.

#![cfg_attr(not(target_env = "gnu"), no_std)]
#![cfg_attr(not(target_env = "gnu"), no_main)]

extern crate alloc;

use core::panic::PanicInfo;

#[cfg(not(target_env = "gnu"))]
use core::alloc::{GlobalAlloc, Layout};
#[cfg(not(target_env = "gnu"))]
use core::sync::atomic::{AtomicUsize, Ordering};

use eclipse_libc::{println, yield_cpu, exit, InputEvent};
use eclipse_ipc::prelude::*;

// --- Allocator (Sólo necesario en el target Eclipse que es no_std) ---
#[cfg(not(target_env = "gnu"))]
const HEAP_SIZE: usize = 1024 * 1024; // 1MB
#[cfg(not(target_env = "gnu"))]
#[repr(align(4096))]
struct Heap([u8; HEAP_SIZE]);
#[cfg(not(target_env = "gnu"))]
static mut HEAP: Heap = Heap([0u8; HEAP_SIZE]);
#[cfg(not(target_env = "gnu"))]
static HEAP_PTR: AtomicUsize = AtomicUsize::new(0);

#[cfg(not(target_env = "gnu"))]
struct StaticAllocator;
#[cfg(not(target_env = "gnu"))]
unsafe impl GlobalAlloc for StaticAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let size = layout.size();
        let current = HEAP_PTR.load(Ordering::SeqCst);
        let aligned = (current + align - 1) & !(align - 1);
        if aligned + size > HEAP_SIZE { return core::ptr::null_mut(); }
        HEAP_PTR.store(aligned + size, Ordering::SeqCst);
        HEAP.0.as_mut_ptr().add(aligned)
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[cfg(not(target_env = "gnu"))]
#[global_allocator]
static ALLOCATOR: StaticAllocator = StaticAllocator;

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
    
    // Loop principal - procesar eventos
    for _ in 0..10 {
        if let Some(msg) = ch.recv() {
            event_count += 1;
            match msg {
                EclipseMessage::Input(ev) => {
                    println!("INPUT_TEST: [MSG] Recibido evento de entrada!");
                    
                    if ev.event_type == 0 {
                        keyboard_count += 1;
                        println!("INPUT_TEST:   - Tipo: TECLADO, Código: {}, Valor: {}\n", ev.code, ev.value);
                    } else if ev.event_type == 1 {
                        mouse_count += 1;
                        println!("INPUT_TEST:   - Tipo: RATÓN (MOVE), Valor: {}\n", ev.value);
                    } else {
                        println!("INPUT_TEST:   - Tipo: OTRO ({}), Valor: {}\n", ev.event_type, ev.value);
                    }
                }
                _ => {
                    println!("INPUT_TEST: [MSG] Recibido mensaje de otro tipo.\n");
                }
            }
        }
        yield_cpu();
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

/// Inyectar eventos en el mock de libc para propósitos de test (HOST ONLY)
#[cfg(target_env = "gnu")]
fn inject_mock_input_events() {
    // Usamos el backdoor del mock de libc para simular llegada de mensajes
    use eclipse_libc::{mock_push_receive};
    
    // 1. Evento de Teclado (Tecla 'A' presionada)
    let kbd_ev = InputEvent {
        device_id: 1,
        event_type: 0,
        code: 30,
        value: 1,
        timestamp: 12345,
    };
    let kbd_data = unsafe { 
        core::slice::from_raw_parts(&kbd_ev as *const _ as *const u8, core::mem::size_of::<InputEvent>())
    };
    mock_push_receive(kbd_data.to_vec(), 500); // 500 es el PID del InputService
    
    // 2. Evento de Ratón (Movimiento X)
    let mouse_ev = InputEvent {
        device_id: 2,
        event_type: 1,
        code: 0,
        value: 100,
        timestamp: 12346,
    };
    let mouse_data = unsafe { 
        core::slice::from_raw_parts(&mouse_ev as *const _ as *const u8, core::mem::size_of::<InputEvent>())
    };
    mock_push_receive(mouse_data.to_vec(), 500);
}

#[cfg(not(target_env = "gnu"))]
fn inject_mock_input_events() {
    // No-op en el target real
}

/// Salir de la aplicación
fn exit_app(code: i32) -> ! {
    println!("INPUT_TEST: Saliendo con código {}\n", code);
    exit(code);
}
