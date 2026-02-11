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

/// Scratch space para guardar RSP de usuario durante syscall entry
static mut USER_RSP_SCRATCH: u64 = 0;

/// MSR Constants
const MSR_EFER: u32 = 0xC0000080;
const MSR_STAR: u32 = 0xC0000081;
const MSR_LSTAR: u32 = 0xC0000082;
const MSR_SFMASK: u32 = 0xC0000084;

unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    asm!("rdmsr", out("eax") low, out("edx") high, in("ecx") msr, options(nomem, nostack, preserves_flags));
    ((high as u64) << 32) | (low as u64)
}

unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high, options(nomem, nostack, preserves_flags));
}

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
        KERNEL_IDT.entries[10].set_handler(exception_10 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[11].set_handler(exception_11 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[12].set_handler(exception_12 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[13].set_handler(exception_13 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[14].set_handler(exception_14 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        
        // Configurar handlers de IRQ (32-47)
        KERNEL_IDT.entries[32].set_handler(irq_0 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[33].set_handler(irq_1 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        
        // Configurar syscall handler (int 0x80)
        // Must be callable from Ring 3 (DPL 3) or it will cause #GP
        const IDT_RING_3: u8 = 0b01100000;
        KERNEL_IDT.entries[0x80].set_handler(syscall_int80 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_3 | IDT_INTERRUPT_GATE);
        
        // --- Habilitar Syscall Instruction ---
        // 1. Enable SCE in EFER
        let efer = rdmsr(MSR_EFER);
        wrmsr(MSR_EFER, efer | 1);

        // 2. Setup STAR
        // 63:48 = Sysret CS (0x08) -> Not used due to GDT layout, we use iretq
        // 47:32 = Syscall CS (0x08) -> Loads CS=0x08 (Kernel Code), SS=0x10 (Kernel Data)
        wrmsr(MSR_STAR, (0x08 << 32) | (0x08 << 48));

        // 3. Setup LSTAR (Entry point)
        wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);

        // 4. Setup SFMASK (Mask Interrupts 0x200)
        wrmsr(MSR_SFMASK, 0x200);
        
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
    
    // Deshabilitar APIC (para volver a modo Legacy PIC)
    disable_apic();

    // Inicializar PIC
    init_pic();
    
    // Inicializar PIT (Timer)
    init_pit();
    
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

/// Inicializar PIT 8253/8254
/// Configura el timer para disparar a ~1000Hz
fn init_pit() {
    // Frecuencia base del PIT: 1.193182 MHz
    // Divisor = 1193182 / Frecuencia deseada
    // Para 1000Hz: 1193182 / 1000 = 1193
    let divisor: u16 = 1193;
    
    unsafe {
        // Puerto 0x43: Command Register
        // 00 (Channel 0) | 11 (Access mode: lo/hi byte) | 011 (Mode 3: Square Wave) | 0 (Binary)
        // 0x36 = 00110110
        outb(0x43, 0x36);
        
        // Puerto 0x40: Channel 0 Data
        // Escribir byte bajo
        outb(0x40, (divisor & 0xFF) as u8);
        // Escribir byte alto
        outb(0x40, ((divisor >> 8) & 0xFF) as u8);
    }
    
    crate::serial::serial_print("[INT] PIT Initialized (1000Hz)\n");
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

#[repr(C, packed)]
pub struct ExceptionContext {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub rbp: u64,
    pub num: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

extern "C" fn exception_handler(context: &ExceptionContext) {
    let mut stats = INTERRUPT_STATS.lock();
    stats.exceptions += 1;
    drop(stats);
    
    crate::serial::serial_print("\n!!! EXCEPTION: ");
    crate::serial::serial_print_dec(context.num);
    crate::serial::serial_print(" Error: ");
    crate::serial::serial_print_hex(context.error_code);
    crate::serial::serial_print(" RIP: ");
    crate::serial::serial_print_hex(context.rip);
    crate::serial::serial_print(" !!!\n");
    
    // Dump all registers
    crate::serial::serial_print("  RAX: "); crate::serial::serial_print_hex(context.rax);
    crate::serial::serial_print(" RBX: "); crate::serial::serial_print_hex(context.rbx);
    crate::serial::serial_print(" RCX: "); crate::serial::serial_print_hex(context.rcx);
    crate::serial::serial_print(" RDX: "); crate::serial::serial_print_hex(context.rdx);
    crate::serial::serial_print("\n");
    crate::serial::serial_print("  RSI: "); crate::serial::serial_print_hex(context.rsi);
    crate::serial::serial_print(" RDI: "); crate::serial::serial_print_hex(context.rdi);
    crate::serial::serial_print(" RBP: "); crate::serial::serial_print_hex(context.rbp);
    crate::serial::serial_print(" RSP: "); crate::serial::serial_print_hex(context.rsp);
    crate::serial::serial_print("\n");
    crate::serial::serial_print("  R8:  "); crate::serial::serial_print_hex(context.r8);
    crate::serial::serial_print(" R9:  "); crate::serial::serial_print_hex(context.r9);
    crate::serial::serial_print(" R10: "); crate::serial::serial_print_hex(context.r10);
    crate::serial::serial_print(" R11: "); crate::serial::serial_print_hex(context.r11);
    crate::serial::serial_print("\n");
    crate::serial::serial_print("  R12: "); crate::serial::serial_print_hex(context.r12);
    crate::serial::serial_print(" R13: "); crate::serial::serial_print_hex(context.r13);
    crate::serial::serial_print(" R14: "); crate::serial::serial_print_hex(context.r14);
    crate::serial::serial_print(" R15: "); crate::serial::serial_print_hex(context.r15);
    crate::serial::serial_print("\n");
    crate::serial::serial_print("  RFL: "); crate::serial::serial_print_hex(context.rflags);
    crate::serial::serial_print(" CS:  "); crate::serial::serial_print_hex(context.cs);
    crate::serial::serial_print(" SS:  "); crate::serial::serial_print_hex(context.ss);
    crate::serial::serial_print("\n");
    
    let cr3: u64;
    let cr2: u64;
    unsafe { 
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)); 
        core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)); 
    }
    crate::serial::serial_print("  Active CR3: ");
    crate::serial::serial_print_hex(cr3);
    crate::serial::serial_print(" CR2: ");
    crate::serial::serial_print_hex(cr2);
    
    if let Some(pid) = crate::process::current_process_id() {
        crate::serial::serial_print(" PID: ");
        crate::serial::serial_print_dec(pid as u64);
    }
    crate::serial::serial_print("\n");
    
    loop { 
        if context.num == 3 { return; } // Breakpoint
        unsafe { asm!("hlt") } 
    }
}

#[unsafe(naked)]
unsafe extern "C" fn common_exception_handler() {
    core::arch::naked_asm!(
        // Stack tiene: [num, error_code, iretq_frame...]
        "push rbp",
        "mov rbp, rsp",
        
        // Guardar GPRs
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
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
        
        // RSP apunta a r15. Offset 0.
        "mov rdi, rsp", // Argumento 1: &ExceptionContext
        
        "and rsp, -16",
        "call {}",
        
        "mov rsp, rbp",
        "sub rsp, 112", // Offset de R15 (14 registros x 8 bytes)
        
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
        "pop rax",
        
        "pop rbp",
        "add rsp, 16", // Limpiar num y error_code
        "iretq",
        sym exception_handler,
    );
}



// Division by zero (#DE)
#[unsafe(naked)]
unsafe extern "C" fn exception_0() {
    core::arch::naked_asm!(
        "push 0", // Dummy error code
        "push 0", // Exception num
        "jmp {}",
        sym common_exception_handler,
    );
}

// Debug (#DB)
#[unsafe(naked)]
unsafe extern "C" fn exception_1() {
    core::arch::naked_asm!(
        "push 0", // Dummy error code
        "push 1", // Exception num
        "jmp {}",
        sym common_exception_handler,
    );
}

// Breakpoint (#BP)
#[unsafe(naked)]
unsafe extern "C" fn exception_3() {
    core::arch::naked_asm!(
        "push 0", // Dummy error code
        "push 3", // Exception num
        "jmp {}",
        sym common_exception_handler,
    );
}

// Overflow (#OF)
#[unsafe(naked)]
unsafe extern "C" fn exception_4() {
    core::arch::naked_asm!(
        "push 0", // Dummy error code
        "push 4", // Exception num
        "jmp {}",
        sym common_exception_handler,
    );
}

// Invalid Opcode (#UD)
#[unsafe(naked)]
unsafe extern "C" fn exception_6() {
    core::arch::naked_asm!(
        "push 0", // Dummy error code
        "push 6", // Exception num
        "jmp {}",
        sym common_exception_handler,
    );
}

// Double Fault (#DF) - con error code
#[unsafe(naked)]
unsafe extern "C" fn exception_8() {
    core::arch::naked_asm!(
        // Error code ya está en el stack
        "push 8", // Exception num
        "jmp {}",
        sym common_exception_handler,
    );
}

// General Protection Fault (#GP) - con error code
#[unsafe(naked)]
unsafe extern "C" fn exception_13() {
    core::arch::naked_asm!(
        // Error code ya está en el stack
        "push 13", // Exception num
        "jmp {}",
        sym common_exception_handler,
    );
}

// Page Fault (#PF) - con error code
#[unsafe(naked)]
unsafe extern "C" fn exception_14() {
    core::arch::naked_asm!(
        // Error code ya está en el stack
        "push 14", // Exception num
        "jmp {}",
        sym common_exception_handler,
    );
}

// Invalid TSS (#TS) - con error code
#[unsafe(naked)]
unsafe extern "C" fn exception_10() {
    core::arch::naked_asm!(
        "push 10",
        "jmp {}",
        sym common_exception_handler,
    );
}

// Segment Not Present (#NP) - con error code
#[unsafe(naked)]
unsafe extern "C" fn exception_11() {
    core::arch::naked_asm!(
        "push 11",
        "jmp {}",
        sym common_exception_handler,
    );
}

// Stack Segment Fault (#SS) - con error code
#[unsafe(naked)]
unsafe extern "C" fn exception_12() {
    core::arch::naked_asm!(
        "push 12",
        "jmp {}",
        sym common_exception_handler,
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

/// Contexto de registros guardados en el stack durante una syscall
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SyscallContext {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub rbp: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

extern "C" fn syscall_handler_rust(
    syscall_num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    context: &mut SyscallContext,
) -> u64 {
    crate::syscalls::syscall_handler(syscall_num, arg1, arg2, arg3, arg4, arg5, context)
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
        
        // Pasar puntero al contexto (RSP apunta a r15, que es el inicio de la estructura)
        // La estructura SyscallContext mapea exactamente el layout del stack desde r15 hasta ss
        "lea rax, [rbp - 112]", // Dirección de r15
        "push rax",      // 7º argumento en el stack
        
        "call {}",
        
        "add rsp, 8", // Limpiar 7º arg
        
        // Restaurar registros GP (ojo: RSP original está en RBP)
        "mov rsp, rbp",
        "sub rsp, 112", // Mover RSP al inicio de los regs pusheados (r15)
        
        // El resultado está en RAX. Queremos que el RAX pusheado sea este resultado
        // Offset de RAX desde RBP es -8.
        "mov [rbp - 8], rax",
        
        // CRITICAL: Restore user data segments BEFORE popping registers
        // We need to do this while RBP is still valid
        "mov ax, 0x23",  // USER_DATA_SELECTOR
        "mov ds, ax",
        "mov es, ax",
        "mov fs, ax",
        "mov gs, ax",
        
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

#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // Syscall entry point (via LSTAR)
        // RCX = User RIP
        // R11 = User RFLAGS
        // RSP = User RSP
        
        // Save User RSP to global scratch
        "mov [rip + {user_rsp}], rsp",
        
        // Load Kernel RSP from TSS (offset 4 is rsp0)
        "mov rsp, [rip + {tss} + 4]",
        
        // Build IRETQ stack frame manually
        // Layout: SS, RSP, RFLAGS, CS, RIP
        
        // 1. SS (User Data = 0x23)
        "mov rax, 0x23",
        "push rax",
        
        // 2. RSP (Saved User RSP)
        "push [rip + {user_rsp}]",
        
        // 3. RFLAGS (Saved in R11)
        "push r11",
        
        // 4. CS (User Code = 0x1B)
        "mov rax, 0x1B",
        "push rax",
        
        // 5. RIP (Saved in RCX)
        "push rcx",
        
        // Now on Kernel Stack with IRETQ frame. Save GPs.
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
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
        
        "mov rbp, rsp",
        "and rsp, -16", // Align
        
        // Map arguments (Linux Syscall ABI -> Rust Handler)
        // Linux: RAX (num), RDI (1), RSI (2), RDX (3), R10 (4), R8 (5), R9 (6)
        // Rust Handler: sys_num, arg1, arg2, arg3, arg4, arg5, context
        
        "mov r9, r8",    // arg5 (from r8)
        "mov r8, r10",   // arg4 (from r10)
        "mov rcx, rdx",  // arg3 (from rdx)
        "mov rdx, rsi",  // arg2 (from rsi)
        "mov rsi, rdi",  // arg1 (from rdi)
        "mov rdi, rax",  // syscall_num
        
        "lea rax, [rbp - 112]", // Context Ptr (address of r15)
        "push rax",      // 7th arg
        
        "call {handler}",
        
        "add rsp, 8", // Pop 7th arg
        "mov rsp, rbp", // Restore stack
        
        // CRITICAL: Restore user data segments BEFORE popping registers
        "mov ax, 0x23",  // USER_DATA_SELECTOR
        "mov ds, ax",
        "mov es, ax",
        "mov fs, ax",
        "mov gs, ax",
        
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
        "pop rax",
        
        "pop rbp",
        
        "iretq",
        
        user_rsp = sym USER_RSP_SCRATCH,
        tss = sym crate::boot::TSS,
        handler = sym syscall_handler_rust,
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

/// Deshabilitar APIC local vía MSR
/// Esto es necesario si UEFI dejó el APIC habilitado, para forzar el uso del PIC legado
fn disable_apic() {
    unsafe {
        use core::arch::asm;
        let msr = 0x1B; // IA32_APIC_BASE
        let low: u32;
        let high: u32;

        // Leer MSR actual
        asm!("rdmsr", out("eax") low, out("edx") high, in("ecx") msr);

        // Si el bit 11 (Enable) está activo, desactivarlo
        if (low & (1 << 11)) != 0 {
            let new_low = low & !(1 << 11);
            asm!("wrmsr", in("ecx") msr, in("eax") new_low, in("edx") high);
            crate::serial::serial_print("[INT] APIC disabled via MSR (Forced Legacy PIC)\n");
        } else {
             crate::serial::serial_print("[INT] APIC was already disabled\n");
        }
    }
}
