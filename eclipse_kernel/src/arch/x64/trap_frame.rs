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
    
    /// Restaura los registros desde el trap frame
    pub unsafe fn restore_registers(&self) {
        // Esta función debe ser implementada en assembly
        // para restaurar los registros del CPU
        core::arch::asm!(
            "mov rax, {rax}",
            "mov rcx, {rcx}",
            "mov rdx, {rdx}",
            "mov rbx, {rbx}",
            "mov rsp, {rsp}",
            "mov rbp, {rbp}",
            "mov rsi, {rsi}",
            "mov rdi, {rdi}",
            "mov r8, {r8}",
            "mov r9, {r9}",
            "mov r10, {r10}",
            "mov r11, {r11}",
            "mov r12, {r12}",
            "mov r13, {r13}",
            "mov r14, {r14}",
            "mov r15, {r15}",
            rax = in(reg) self.rax,
            rcx = in(reg) self.rcx,
            rdx = in(reg) self.rdx,
            rbx = in(reg) self.rbx,
            rsp = in(reg) self.rsp,
            rbp = in(reg) self.rbp,
            rsi = in(reg) self.rsi,
            rdi = in(reg) self.rdi,
            r8 = in(reg) self.r8,
            r9 = in(reg) self.r9,
            r10 = in(reg) self.r10,
            r11 = in(reg) self.r11,
            r12 = in(reg) self.r12,
            r13 = in(reg) self.r13,
            r14 = in(reg) self.r14,
            r15 = in(reg) self.r15,
        );
    }
}

/// Macro para crear un trap frame desde assembly
#[macro_export]
macro_rules! create_trap_frame {
    () => {
        core::arch::asm!(
            "push rax",
            "push rcx", 
            "push rdx",
            "push rbx",
            "push rbp",
            "push rsi",
            "push rdi",
            "push r8",
            "push r9",
            "push r10",
            "push r11",
            "push r12",
            "push r13",
            "push r14",
            "push r15",
            "pushfq",
        );
    };
}

/// Handler de excepciones en assembly que llama a Rust
#[unsafe(naked)]
pub extern "C" fn ki_invalid_opcode_fault() {
    core::arch::naked_asm!(
        // Crear trap frame
        "push rax",
        "push rcx",
        "push rdx", 
        "push rbx",
        "push rbp",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "pushfq",
        
        // Llamar al handler de Rust
        "mov rdi, rsp",  // trap_frame
        "mov rsi, 6",    // exception_code (Invalid Opcode)
        "call {handler}",
        
        // Restaurar y retornar
        "add rsp, 8 * 16", // Limpiar stack (15 registros + flags)
        "iretq",
        
        handler = sym handle_exception_placeholder
    );
}

/// Placeholder para el manejador de excepciones
#[no_mangle]
pub extern "C" fn handle_exception_placeholder() {
    // TODO: Implementar manejo de excepciones
}
