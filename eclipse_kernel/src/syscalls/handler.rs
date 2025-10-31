//! Manejador de syscalls para Eclipse OS
//! 
//! Este módulo implementa el manejador principal de syscalls que intercepta
//! las llamadas al sistema desde el espacio de usuario.

use crate::debug::serial_write_str;
use super::{SyscallArgs, SyscallResult, SyscallError, SyscallRegistry};

/// Contexto de syscall
pub struct SyscallContext {
    pub rax: u64,    // Número de syscall
    pub rdi: u64,    // Argumento 1
    pub rsi: u64,    // Argumento 2
    pub rdx: u64,    // Argumento 3
    pub rcx: u64,    // Argumento 4 (x64 usa r10 en lugar de rcx)
    pub r8: u64,     // Argumento 5
    pub r9: u64,     // Argumento 6
    pub r10: u64,    // Argumento 4 real en x64
    pub r11: u64,    // Registro temporal
    pub r12: u64,    // Registro preservado
    pub r13: u64,    // Registro preservado
    pub r14: u64,    // Registro preservado
    pub r15: u64,    // Registro preservado
    pub rbp: u64,    // Frame pointer
    pub rbx: u64,    // Registro preservado
    pub rsp: u64,    // Stack pointer
    pub rip: u64,    // Instruction pointer
    pub rflags: u64, // Flags
    pub fs_base: u64, // Base del segmento FS
    pub gs_base: u64, // Base del segmento GS
}

impl SyscallContext {
    /// Crear contexto de syscall desde registros
    pub fn from_registers(
        rax: u64, rdi: u64, rsi: u64, rdx: u64, r10: u64, r8: u64, r9: u64,
        r11: u64, r12: u64, r13: u64, r14: u64, r15: u64, rbp: u64, rbx: u64,
        rsp: u64, rip: u64, rflags: u64, fs_base: u64, gs_base: u64,
    ) -> Self {
        Self {
            rax, rdi, rsi, rdx, rcx: 0, r8, r9, r10, r11, r12, r13, r14, r15,
            rbp, rbx, rsp, rip, rflags, fs_base, gs_base,
        }
    }

    /// Obtener argumentos de syscall
    pub fn get_args(&self) -> SyscallArgs {
        SyscallArgs::from_registers(self.rdi, self.rsi, self.rdx, self.r10, self.r8, self.r9)
    }
}

/// Manejador principal de syscalls
pub struct SyscallHandler {
    registry: SyscallRegistry,
}

impl SyscallHandler {
    /// Crear nuevo manejador de syscalls
    pub fn new(registry: SyscallRegistry) -> Self {
        Self { registry }
    }

    /// Manejar una syscall
    pub fn handle_syscall(&self, context: &mut SyscallContext) -> u64 {
        let syscall_num = context.rax;
        let args = context.get_args();

        serial_write_str(&alloc::format!("SYSCALL_HANDLER: Procesando syscall {}\n", syscall_num));

        // Ejecutar la syscall
        let result = self.registry.execute(syscall_num as usize, &args);

        // Establecer el resultado en RAX
        match result {
            SyscallResult::Success(value) => {
                serial_write_str(&alloc::format!("SYSCALL_HANDLER: Syscall {} exitosa, retornando {}\n", syscall_num, value));
                value
            }
            SyscallResult::Error(error) => {
                let errno = error.to_errno();
                serial_write_str(&alloc::format!("SYSCALL_HANDLER: Syscall {} falló con error {:?} (errno: {})\n", syscall_num, error, errno));
                errno as u64
            }
        }
    }

    /// Verificar si un syscall es válido
    pub fn is_valid_syscall(&self, syscall_num: u64) -> bool {
        syscall_num < super::SYSCALL_COUNT as u64
    }

    /// Obtener información de debug sobre una syscall
    pub fn debug_syscall(&self, syscall_num: u64, args: &SyscallArgs) {
        serial_write_str(&alloc::format!("SYSCALL_DEBUG: Número: {}\n", syscall_num));
        serial_write_str(&alloc::format!("SYSCALL_DEBUG: arg0: 0x{:x}\n", args.arg0));
        serial_write_str(&alloc::format!("SYSCALL_DEBUG: arg1: 0x{:x}\n", args.arg1));
        serial_write_str(&alloc::format!("SYSCALL_DEBUG: arg2: 0x{:x}\n", args.arg2));
        serial_write_str(&alloc::format!("SYSCALL_DEBUG: arg3: 0x{:x}\n", args.arg3));
        serial_write_str(&alloc::format!("SYSCALL_DEBUG: arg4: 0x{:x}\n", args.arg4));
        serial_write_str(&alloc::format!("SYSCALL_DEBUG: arg5: 0x{:x}\n", args.arg5));
    }
}

/// Función de entrada de syscall (llamada desde el espacio de usuario)
/// Esta función debe ser llamada desde el espacio de usuario usando la instrucción SYSCALL
#[no_mangle]
pub extern "C" fn syscall_entry(
    rax: u64, rdi: u64, rsi: u64, rdx: u64, r10: u64, r8: u64, r9: u64,
    r11: u64, r12: u64, r13: u64, r14: u64, r15: u64, rbp: u64, rbx: u64,
    rsp: u64, rip: u64, rflags: u64, fs_base: u64, gs_base: u64,
) -> u64 {
    serial_write_str("SYSCALL_ENTRY: Interceptada syscall desde espacio de usuario\n");

    // Crear contexto de syscall
    let mut context = SyscallContext::from_registers(
        rax, rdi, rsi, rdx, r10, r8, r9, r11, r12, r13, r14, r15,
        rbp, rbx, rsp, rip, rflags, fs_base, gs_base,
    );

    // Obtener el manejador de syscalls (debe estar inicializado globalmente)
    // Por ahora, creamos uno temporal para testing
    let registry = super::init_syscalls();
    let handler = SyscallHandler::new(registry);

    // Manejar la syscall
    handler.handle_syscall(&mut context)
}

/// Configurar la MSR para syscalls
pub fn setup_syscall_msr() {
    serial_write_str("SYSCALL: Configurando MSR para syscalls\n");

    unsafe {
        use x86_64::registers::model_specific::Msr;

        // Configurar LSTAR (dirección de entrada de syscall)
        let syscall_entry_addr = syscall_entry as u64;
        Msr::new(0xC0000082).write(syscall_entry_addr); // LSTAR

        // Configurar STAR (selector de segmento para syscall)
        // CS = 0x08 (kernel), SS = 0x10 (kernel)
        Msr::new(0xC0000081).write(0x0010000800000000); // STAR

        // Configurar SFMASK (máscara de flags para syscall)
        // Limpiar IF (interrupt flag) y DF (direction flag)
        Msr::new(0xC0000084).write(0x00000300); // SFMASK

        serial_write_str(&alloc::format!("SYSCALL: LSTAR configurado en 0x{:x}\n", syscall_entry_addr));
        serial_write_str("SYSCALL: STAR configurado\n");
        serial_write_str("SYSCALL: SFMASK configurado\n");
    }
}

/// Habilitar syscalls en el procesador
pub fn enable_syscalls() {
    serial_write_str("SYSCALL: Habilitando syscalls en el procesador\n");

    unsafe {
        use x86_64::registers::model_specific::Efer;

        // Habilitar SYSCALL/SYSRET
        Efer::update(|efer| {
            efer.set(x86_64::registers::model_specific::EferFlags::SYSTEM_CALL_EXTENSIONS, true);
        });

        serial_write_str("SYSCALL: SYSCALL/SYSRET habilitado\n");
    }
}

/// Inicializar el sistema de syscalls
pub fn init_syscall_system() -> SyscallHandler {
    serial_write_str("SYSCALL: Inicializando sistema de syscalls\n");

    // Configurar MSRs
    setup_syscall_msr();

    // Habilitar syscalls
    enable_syscalls();

    // Crear registro de syscalls
    let registry = super::init_syscalls();

    // Crear manejador
    let handler = SyscallHandler::new(registry);

    serial_write_str("SYSCALL: Sistema de syscalls inicializado completamente\n");
    
    // Inicializar syscalls USB
    serial_write_str("SYSCALL: Inicializando syscalls USB...\n");
    if let Err(e) = crate::syscalls::usb::init_usb_syscalls() {
        serial_write_str(&alloc::format!("SYSCALL: Error al inicializar USB syscalls: {}\n", e));
    } else {
        serial_write_str("SYSCALL: USB syscalls inicializados\n");
    }
    
    handler
}

/// Función de prueba para syscalls
pub fn test_syscalls() {
    serial_write_str("SYSCALL: Iniciando pruebas de syscalls\n");

    // Crear manejador temporal
    let registry = super::init_syscalls();
    let handler = SyscallHandler::new(registry);

    // Probar syscall exit
    let args = SyscallArgs::from_registers(0, 0, 0, 0, 0, 0);
    let result = handler.registry.execute(0, &args); // sys_exit
    serial_write_str(&alloc::format!("SYSCALL_TEST: sys_exit result: {:?}\n", result));

    // Probar syscall write
    let args = SyscallArgs::from_registers(1, 0, 10, 0, 0, 0); // fd=1, count=10
    let result = handler.registry.execute(1, &args); // sys_write
    serial_write_str(&alloc::format!("SYSCALL_TEST: sys_write result: {:?}\n", result));

    // Probar syscall getpid
    let args = SyscallArgs::from_registers(0, 0, 0, 0, 0, 0);
    let result = handler.registry.execute(9, &args); // sys_getpid
    serial_write_str(&alloc::format!("SYSCALL_TEST: sys_getpid result: {:?}\n", result));

    serial_write_str("SYSCALL: Pruebas de syscalls completadas\n");
}

