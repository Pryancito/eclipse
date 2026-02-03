//! Sistema de interrupciones del microkernel
//! 
//! Implementa IDT completa con handlers de excepciones e IRQs

use core::arch::asm;
use spin::Mutex;

/// Estadísticas de interrupciones
pub struct InterruptStats {
    pub exceptions: u64,
    pub irqs: u64,
    pub timer_ticks: u64,
}

static INTERRUPT_STATS: Mutex<InterruptStats> = Mutex::new(InterruptStats {
    exceptions: 0,
    irqs: 0,
    timer_ticks: 0,
});

/// Descriptor de interrupción en la IDT
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    flags: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn new() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            flags: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }
    
    fn set_handler(&mut self, handler: u64, selector: u16, flags: u8) {
        self.offset_low = (handler & 0xFFFF) as u16;
        self.offset_mid = ((handler >> 16) & 0xFFFF) as u16;
        self.offset_high = ((handler >> 32) & 0xFFFFFFFF) as u32;
        self.selector = selector;
        self.flags = flags;
        self.ist = 0;
        self.reserved = 0;
    }
}

/// IDT con 256 entradas
#[repr(C, align(16))]
struct Idt {
    entries: [IdtEntry; 256],
}

impl Idt {
    const fn new() -> Self {
        Self {
            entries: [IdtEntry::new(); 256],
        }
    }
}

/// Descriptor de la IDT
#[repr(C, packed)]
struct IdtDescriptor {
    limit: u16,
    base: u64,
}

/// IDT estática del kernel
static mut KERNEL_IDT: Idt = Idt::new();

/// Flags para descriptores de interrupción
const IDT_PRESENT: u8 = 0b10000000;
const IDT_RING_0: u8 = 0b00000000;
const IDT_INTERRUPT_GATE: u8 = 0b00001110;

/// Inicializar el sistema de interrupciones
pub fn init() {
    unsafe {
        // Configurar handlers de excepciones (0-31)
        KERNEL_IDT.entries[0].set_handler(exception_0 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[1].set_handler(exception_1 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[3].set_handler(exception_3 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[4].set_handler(exception_4 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[6].set_handler(exception_6 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[8].set_handler(exception_8 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[13].set_handler(exception_13 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[14].set_handler(exception_14 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        
        // Configurar handlers de IRQ (32-47)
        KERNEL_IDT.entries[32].set_handler(irq_0 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[33].set_handler(irq_1 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        
        // Configurar syscall handler (int 0x80)
        // Must be callable from Ring 3 (DPL 3) or it will cause #GP
        const IDT_RING_3: u8 = 0b01100000;
        KERNEL_IDT.entries[0x80].set_handler(syscall_int80 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_3 | IDT_INTERRUPT_GATE);
        
        // Cargar IDT
        let idt_descriptor = IdtDescriptor {
            limit: (core::mem::size_of::<Idt>() - 1) as u16,
            base: &raw const KERNEL_IDT as *const _ as u64,
        };
        
        asm!(
            "lidt [{}]",
            in(reg) &idt_descriptor,
            options(nostack, preserves_flags)
        );
    }
    
    // Inicializar PIC
    init_pic();
    
    // Habilitar interrupciones
    unsafe {
        asm!("sti", options(nomem, nostack));
        crate::serial::serial_print("[INT] Interrupts ENABLED\n");
    }
}

/// Inicializar PIC 8259
fn init_pic() {
    unsafe {
        // ICW1 - Iniciar PIC
        outb(0x20, 0x11);
        outb(0xA0, 0x11);
        
        // ICW2 - Mapear IRQs
        outb(0x21, 0x20); // Master: IRQ 0-7 -> INT 0x20-0x27
        outb(0xA1, 0x28); // Slave: IRQ 8-15 -> INT 0x28-0x2F
        
        // ICW3 - Configurar cascading
        outb(0x21, 0x04); // Master: slave en IRQ2
        outb(0xA1, 0x02); // Slave: cascade identity
        
        // ICW4 - Modo 8086
        outb(0x21, 0x01);
        outb(0xA1, 0x01);
        
        // Habilitar solo timer y teclado
        outb(0x21, 0xFC); // Mask: 11111100 (solo IRQ0 y IRQ1)
        outb(0xA1, 0xFF); // Mask todo el slave
    }
}

/// Enviar End Of Interrupt al PIC
#[inline]
pub fn send_eoi(irq: u8) {
    unsafe {
        if irq >= 8 {
            outb(0xA0, 0x20);
        }
        outb(0x20, 0x20);
    }
}

/// Escribir a puerto I/O
#[inline]
unsafe fn outb(port: u16, value: u8) {
    asm!(
        "out dx, al",
        in("dx") port,
        in("al") value,
        options(nomem, nostack, preserves_flags)
    );
}

/// Leer de puerto I/O
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    asm!(
        "in al, dx",
        in("dx") port,
        out("al") value,
        options(nomem, nostack, preserves_flags)
    );
    value
}

// ============================================================================
// Exception Handlers
// ============================================================================

extern "C" fn exception_handler(num: u64, error_code: u64, rip: u64) {
    let mut stats = INTERRUPT_STATS.lock();
    stats.exceptions += 1;
    drop(stats);
    
    crate::serial::serial_print("EXCEPTION: ");
    crate::serial::serial_print_dec(num);
    crate::serial::serial_print(" Error: ");
    crate::serial::serial_print_hex(error_code);
    crate::serial::serial_print(" RIP: ");
    crate::serial::serial_print_hex(rip);
    crate::serial::serial_print("\n");
    
    // Halt to avoid loop
    if num == 14 { // Page Fault
        let cr2: u64;
        unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)); }
        crate::serial::serial_print("CR2: ");
        crate::serial::serial_print_hex(cr2);
        crate::serial::serial_print("\n");
    }
    
    loop { 
        if num == 3 { return; } // Breakpoint should return
        unsafe { asm!("hlt") } 
    }
}

// Division by zero (#DE)
#[unsafe(naked)]
unsafe extern "C" fn exception_0() {
    core::arch::naked_asm!(
        // Push dummy error code
        "push 0",
        "push 0",
        
        // Pass arguments in registers (System V AMD64 ABI)
        // RDI = Exception Number (0)
        // RSI = Error Code (0)
        "pop rdi", // Exception Number (Top of Stack)
        "pop rsi", // Error Code (Next)
        "mov rdx, [rsp]", // Peek RIP
        
        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",

        "call {}",
        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym exception_handler,
    );
}

// Debug (#DB)
#[unsafe(naked)]
unsafe extern "C" fn exception_1() {
    core::arch::naked_asm!(
        "push 0",
        "push 1",
        
        // Arguments: RDI=Num, RSI=Error, RDX=RIP
        "pop rdi", // Exception Number
        "pop rsi", // Error Code
        "mov rdx, [rsp]", // Peek RIP
        
        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",

        "call {}",
        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym exception_handler,
    );
}

// Breakpoint (#BP)
#[unsafe(naked)]
unsafe extern "C" fn exception_3() {
    core::arch::naked_asm!(
        "push 0",
        "push 3",
        
        "pop rdi", // Exception Number
        "pop rsi", // Error Code
        "mov rdx, [rsp]", // Peek RIP

        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",

        "call {}",
        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym exception_handler,
    );
}

// Overflow (#OF)
#[unsafe(naked)]
unsafe extern "C" fn exception_4() {
    core::arch::naked_asm!(
        "push 0",
        "push 4",
        
        "pop rdi", // Exception Number
        "pop rsi", // Error Code
        "mov rdx, [rsp]", // Peek RIP

        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",

        "call {}",
        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym exception_handler,
    );
}

// Invalid Opcode (#UD)
#[unsafe(naked)]
unsafe extern "C" fn exception_6() {
    core::arch::naked_asm!(
        "push 0",
        "push 6",
        
        "pop rdi", // Exception Number
        "pop rsi", // Error Code
        "mov rdx, [rsp]", // Peek RIP

        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",

        "call {}",
        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym exception_handler,
    );
}

// Double Fault (#DF) - con error code
#[unsafe(naked)]
unsafe extern "C" fn exception_8() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",
        // Error code ya está en el stack (pusheado por CPU)
        "pop rsi", // Pop error code into RSI
        "mov rdi, 8", // Exception number into RDI
        "mov rdx, [rsp]", // Peek RIP
        
        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",

        "call {}",
        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym exception_handler,
    );
}

// General Protection Fault (#GP) - con error code
#[unsafe(naked)]
unsafe extern "C" fn exception_13() {
    core::arch::naked_asm!(
        "pop rsi", // Error Code
        "mov rdi, 13", // Exception Number
        "mov rdx, [rsp]", // Peek RIP

        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",
        
        "call {}",
        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym exception_handler,
    );
}

// Page Fault (#PF) - con error code
#[unsafe(naked)]
unsafe extern "C" fn exception_14() {
    core::arch::naked_asm!(
        "pop rsi", // Error Code
        "mov rdi, 14", // Exception Number
        "mov rdx, [rsp]", // Peek RIP

        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",
        
        "call {}",
        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym exception_handler,
    );
}

// ============================================================================
// IRQ Handlers
// ============================================================================

extern "C" fn timer_handler() {
    let mut stats = INTERRUPT_STATS.lock();
    stats.timer_ticks += 1;
    stats.irqs += 1;
    drop(stats);
    
    // Send EOI first to allow other interrupts (or this one if re-enabled) to fire
    send_eoi(0);
    
    // Llamar al scheduler si está disponible
    crate::scheduler::tick();
}

#[unsafe(naked)]
unsafe extern "C" fn irq_0() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "call {}",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym timer_handler,
    );
}

extern "C" fn keyboard_handler() {
    unsafe {
        let _scancode = inb(0x60);
    }
    
    let mut stats = INTERRUPT_STATS.lock();
    stats.irqs += 1;
    drop(stats);
    
    send_eoi(1);
}

#[unsafe(naked)]
unsafe extern "C" fn irq_1() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",
        "push rax",
        "push rcx",
        "push rdx",
        "call {}",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym keyboard_handler,
    );
}

/// Obtener estadísticas de interrupciones
pub fn get_stats() -> InterruptStats {
    let stats = INTERRUPT_STATS.lock();
    InterruptStats {
        exceptions: stats.exceptions,
        irqs: stats.irqs,
        timer_ticks: stats.timer_ticks,
    }
}

// ============================================================================
// Syscall Handler (int 0x80)
// ============================================================================

extern "C" fn syscall_handler_rust(
    syscall_num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    frame_ptr: u64,
) -> u64 {
    crate::syscalls::syscall_handler(syscall_num, arg1, arg2, arg3, arg4, arg5, frame_ptr)
}

#[unsafe(naked)]
unsafe extern "C" fn syscall_int80() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        
        // Guardar registros GP (antes de alinear stack para tener offsets fijos desde RBP)
        "push rax", // [rbp - 8]
        "push rbx", // [rbp - 16]
        "push rcx", // [rbp - 24]
        "push rdx", // [rbp - 32]
        "push rsi", // [rbp - 40]
        "push rdi", // [rbp - 48]
        "push r8",  // [rbp - 56]
        "push r9",  // [rbp - 64]
        "push r10", // [rbp - 72]
        "push r11", // [rbp - 80]
        "push r12", // [rbp - 88]
        "push r13", // [rbp - 96]
        "push r14", // [rbp - 104]
        "push r15", // [rbp - 112]
        
        "and rsp, -16", // Alinear stack para SysV ABI
        
        // Los argumentos de syscall de x86_64 suelen ser:
        // rax = num
        // rdi, rsi, rdx, r10, r8, r9
        
        // Mapear argumentos para la función Rust
        // RDI (arg 0) = syscall_num (RAX)
        // RSI (arg 1) = arg1 (RDI)
        // RDX (arg 2) = arg2 (RSI)
        // RCX (arg 3) = arg3 (RDX)
        // R8  (arg 4) = arg4 (R10)
        // R9  (arg 5) = arg5 (R8)
        
        "mov r9, r8",    // arg5 (from r8)
        "mov r8, r10",   // arg4 (from r10)
        "mov rcx, rdx",  // arg3 (from rdx)
        "mov rdx, rsi",  // arg2 (from rsi)
        "mov rsi, rdi",  // arg1 (from rdi)
        "mov rdi, rax",  // syscall_num
        
        // Pasar RBP (puntero al frame) como 7º argumento
        "push rbp",
        
        "call {}",
        
        "add rsp, 8", // Limpiar 7º arg
        
        // Restaurar registros GP (ojo: RSP original está en RBP)
        "mov rsp, rbp",
        "sub rsp, 112", // Mover RSP al inicio de los regs pusheados (r15)
        
        // El resultado está en RAX. Queremos que el RAX pusheado sea este resultado
        // Offset de RAX desde RBP es -8.
        "mov [rbp - 8], rax",
        
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",  // Ahora rax tiene el valor de retorno
        
        "pop rbp",
        "iretq",
        sym syscall_handler_rust,
    );
}

/// Trampolín para procesos hijos recién creados vía fork
/// 
/// Esta función se encarga de restaurar el contexto de usuario y saltar 
/// a Ring 3 usando los valores guardados en el PCB durante el fork.
#[unsafe(naked)]
pub unsafe extern "C" fn fork_child_trampoline() -> ! {
    core::arch::naked_asm!(
        // Bloquear interrupciones mientras preparamos el salto
        "cli",
        
        // Restaurar selectores de datos
        "push rax",
        "mov ax, 0x23", // USER_DATA_SELECTOR
        "mov ds, ax",
        "mov es, ax",
        "mov fs, ax",
        "mov gs, ax",
        "pop rax",
        
        // El scheduler ya restauró los registros GP (rax, rbx, rsi, etc.)
        // El stack (RSP) ya apunta al frame IRETQ pushgeado por fork_process.
        
        // ¡Salto a Userspace!
        "iretq",
    );
}
