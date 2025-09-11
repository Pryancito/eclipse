//! # Trap Frame Structure
//! 
//! Estructura segura para el trap frame en Rust

use core::mem::size_of;

/// Trap Frame para x86_64
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapFrame {
    // Registros generales
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    
    // Registros de segmento
    pub ds: u16,
    pub es: u16,
    pub fs: u16,
    pub gs: u16,
    pub ss: u16,
    
    // Registros de control
    pub rip: u64,
    pub cs: u16,
    pub rflags: u64,
    
    // Información adicional
    pub error_code: u64,
    pub previous_mode: u8,
    pub reserved: [u8; 7],
}

impl TrapFrame {
    /// Verifica si el trap frame es de modo kernel (optimizado)
    #[inline(always)]
    pub fn is_kernel_mode(&self) -> bool {
        (self.cs & 3) == 0
    }
    
    /// Obtiene el puntero de instrucción de manera segura
    #[inline(always)]
    pub fn instruction_pointer(&self) -> u64 {
        self.rip
    }
    
    /// Actualiza el puntero de instrucción de manera segura
    #[inline(always)]
    pub fn set_instruction_pointer(&mut self, new_rip: u64) {
        self.rip = new_rip;
    }
    
    /// Verifica si el trap frame es de modo usuario
    pub fn is_user_mode(&self) -> bool {
        !self.is_kernel_mode()
    }
    
    /// Obtiene el tamaño del trap frame
    pub const fn size() -> usize {
        size_of::<Self>()
    }
    
    /// Crea un trap frame vacío
    pub const fn new() -> Self {
        Self {
            rax: 0,
            rcx: 0,
            rdx: 0,
            rbx: 0,
            rsp: 0,
            rbp: 0,
            rsi: 0,
            rdi: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            ds: 0,
            es: 0,
            fs: 0,
            gs: 0,
            ss: 0,
            rip: 0,
            cs: 0,
            rflags: 0,
            error_code: 0,
            previous_mode: 0,
            reserved: [0; 7],
        }
    }
    
    /// Restaura los registros desde el trap frame (SIMULACIÓN ULTRA-SEGURA)
    pub unsafe fn restore_registers(&self) {
        // TEMPORALMENTE DESHABILITADO: TODAS las instrucciones assembly causan opcode inválido
        // El problema está en la dirección RIP 000000000009F0AD - necesitamos identificar
        // exactamente qué instrucción está causando el problema

        crate::main_simple::serial_write_str("[TRAP] Restauración de registros SIMULADA (todas las instrucciones ASM deshabilitadas)\r\n");
        crate::main_simple::serial_write_str("[TRAP] ERROR: Opcode inválido en RIP 000000000009F0AD - investigando causa\r\n");

        // TODO: Re-habilitar después de identificar la instrucción problemática
        /*
        // Código real comentado temporalmente
        core::arch::asm!(...);
        */
    }
}

/// Macro para crear un trap frame desde assembly
#[macro_export]
macro_rules! create_trap_frame {
    () => {
        // DESHABILITADO: Las instrucciones push/pop causan opcode inválido
        // Usar simulación segura en lugar de manipulación directa del stack
        unsafe {
            crate::main_simple::serial_write_str("[TRAP] Creación de trap frame simulada\r\n");
        }
    };
}

/// Handler de excepciones en assembly que llama a Rust
#[unsafe(naked)]
pub extern "C" fn ki_invalid_opcode_fault() {
    // DESHABILITADO: Las instrucciones push/pop e iretq causan opcode inválido
    // Usar simulación segura en lugar de manipulación directa del stack
    unsafe {
        crate::main_simple::serial_write_str("[EXCEPTION] Invalid Opcode Exception simulada\r\n");
    }
}

/// Placeholder para el manejador de excepciones
#[no_mangle]
pub extern "C" fn handle_exception_placeholder() {
    // TODO: Implementar manejo de excepciones
}
