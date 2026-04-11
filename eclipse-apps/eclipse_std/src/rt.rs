//! Runtime (crt0) para Eclipse OS
//!
//! Punto de entrada real que el kernel invoca al cargar un ELF. Prepara el
//! entorno (stack, heap, notificación a init) y llama a la `main()` del usuario.
//! Los pánicos son manejados por el panic_handler, que llama a exit(1).

use core::sync::atomic::{AtomicU64, Ordering};

/// argc pasado por el kernel en el stack (System V ABI: [RSP+0]).
/// Guardado aquí para uso futuro (p. ej. std::env::args).
static ARGC: AtomicU64 = AtomicU64::new(0);

/// Devuelve el número de argumentos con que el kernel arrancó el proceso (por ahora suele ser 0).
#[inline(always)]
pub fn argc() -> u64 {
    ARGC.load(Ordering::Relaxed)
}

/// Lee argc del stack tal como lo dejó el kernel.
/// Layout: [RSP+0] = argc, [RSP+8] = argv[0], ...
#[inline(always)]
pub unsafe fn read_argc_from_stack(rsp: *const u64) -> u64 {
    rsp.read()
}

/// Inicializa el runtime: heap (Box/Vec) y notificación READY/HEART a init (PID 1).
/// TLS no inicializado por ahora.
pub fn init_runtime() {
    crate::heap::init_heap();
    // Variables de entorno por defecto para todos los procesos de Eclipse OS
    crate::env::set_var("TERM", "xterm-256color");
    crate::env::set_var("HOME", "/");
    crate::env::set_var("PATH", "/bin");
    // Leer argv del kernel (registrado por el padre al hacer spawn)
    crate::env::init_args();
    unsafe {
        // En Eclipse OS, SYS_SEND (3) requiere un msg_type. Usamos 0 para READY/HEART.
        let _ = crate::libc::eclipse_send(1, 0, b"READY\0".as_ptr() as *const crate::ffi::c_void, 6, 0);
        let _ = crate::libc::eclipse_send(1, 0, b"HEART\0".as_ptr() as *const crate::ffi::c_void, 6, 0);
    }
}

/// Punto de entrada real (crt0). El kernel salta aquí (ELF entry = _start).
///
/// 1. Lee argc del stack (antes de modificar RSP).
/// 2. Alinea RSP (x86-64 ABI).
/// 3. Inicializa heap y notifica a init.
/// 4. Llama a la `main()` del usuario (símbolo `main`).
/// 5. Sale con el código de retorno de main (syscall exit).
///
/// Si `main()` hace panic, el panic_handler llama a exit(1) y no se vuelve aquí.
#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    // 1. Leer argc del stack (layout del kernel: [RSP+0] = argc)
    let rsp_val: u64;
    core::arch::asm!("mov {}, rsp", out(reg) rsp_val, options(nomem, nostack, preserves_flags));
    let argc = read_argc_from_stack(rsp_val as *const u64);
    ARGC.store(argc, Ordering::Relaxed);

    // 2. Alineación de stack (System V ABI: RSP % 16 == 0)
    core::arch::asm!("and rsp, -16", options(nomem, nostack, preserves_flags));

    // 3. Inicializar heap y avisar a init (READY/HEART)
    init_runtime();

    // 4. Llamar a la main() del usuario (definida en la aplicación)
    // El compilador genera una función "main" (estilo C) que llama a lang_start.
    // Pasamos argc y el puntero a argv que está justo después en el stack.
    let argv_ptr = (rsp_val + 8) as *const *const u8;
    let exit_code = main(argc as isize, argv_ptr);

    // 5. Devolver control al kernel con el código de salida (nunca retorna)
    crate::libc::exit(exit_code);
}

// Símbolo que define la aplicación; el linker lo resuelve con la fn main() generada por el compilador.
extern "C" {
    fn main(argc: isize, argv: *const *const u8) -> i32;
}
