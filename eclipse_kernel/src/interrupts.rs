//! Sistema de interrupciones del microkernel
//! 
//! Implementa IDT completa con handlers de excepciones e IRQs

use core::arch::asm;
use spin::Mutex;

use core::sync::atomic::{AtomicU64, Ordering};

/// Estadísticas de interrupciones
pub struct InterruptStats {
    pub exceptions: u64,
    pub irqs: u64,
}

static INTERRUPT_STATS: Mutex<InterruptStats> = Mutex::new(InterruptStats {
    exceptions: 0,
    irqs: 0,
});

static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

/// Get current timer ticks (1 tick = 1ms at 1000Hz)
pub fn ticks() -> u64 {
    TIMER_TICKS.load(Ordering::Relaxed)
}

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

/// IRQ handler function type
type IrqHandler = fn();

/// IRQ handler table for IRQs 0-15 (interrupts 32-47)
static mut IRQ_HANDLERS: [Option<IrqHandler>; 16] = [None; 16];

/// IDT vector for Local APIC periodic timer (used by all CPUs for scheduling).
/// Must not coincide with the LAPIC spurious interrupt vector (0xFF set in SVR).
pub const APIC_TIMER_VECTOR: u8 = 0xFE;

/// IDT vector for inter-processor TLB shootdown.
/// Fired on all APs when the BSP modifies shared page table entries so each CPU
/// flushes its local TLB.  Must differ from both the spurious (0xFF) and timer (0xFE) vectors.
pub const TLB_SHOOTDOWN_VECTOR: u8 = 0xFD;

/// IDT vector for rescheduling IPI.
/// Sent by a CPU when it enqueues a process to notify other CPUs (e.g. idle ones)
/// that they should call schedule() to pick up the new task.
pub const RESCHEDULE_IPI_VECTOR: u8 = 0xFC;
pub const GPU_INTERRUPT_VECTOR: u8 = 0x40;
pub const USB_INTERRUPT_VECTOR: u8 = 0x41;

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
        // NMI (Non-Maskable Interrupt, vector 2).  Without a registered handler the CPU looks up
        // IDT[2] and finds Present=0, which causes a #NP (vector 11) with error code 0x12 —
        // masking the real NMI source and printing a spurious BSOD.  Install a minimal stub that
        // routes through the common exception handler so NMIs are at least logged.
        KERNEL_IDT.entries[2].set_handler(exception_2 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[3].set_handler(exception_3 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[4].set_handler(exception_4 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[6].set_handler(exception_6 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[8].set_handler(exception_8 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        // Use IST 1 so the #DF handler runs on a dedicated 8 KB stack (set in load_gdt).
        // Without IST a double-fault caused by a stack overflow would immediately triple-fault
        // because the CPU would try to push the exception frame onto the same bad stack.
        KERNEL_IDT.entries[8].ist = 1;
        KERNEL_IDT.entries[10].set_handler(exception_10 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[11].set_handler(exception_11 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[12].set_handler(exception_12 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[13].set_handler(exception_13 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[14].set_handler(exception_14 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        
        // Configurar handlers de IRQ (32-47)
        KERNEL_IDT.entries[32].set_handler(irq_0 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[33].set_handler(irq_1 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        KERNEL_IDT.entries[44].set_handler(irq_12 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE);
        
        // Local APIC timer (vector 0xFE) – fires on every CPU that calls apic::init_timer()
        KERNEL_IDT.entries[APIC_TIMER_VECTOR as usize].set_handler(
            apic_timer_irq as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE,
        );

        // TLB shootdown IPI (vector 0xFD) – sent by BSP to flush TLBs on all APs
        KERNEL_IDT.entries[TLB_SHOOTDOWN_VECTOR as usize].set_handler(
            tlb_shootdown_irq as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE,
        );

        // Reschedule IPI (vector 0xFC) – sent to notify cores of new ready processes
        KERNEL_IDT.entries[RESCHEDULE_IPI_VECTOR as usize].set_handler(
            reschedule_irq as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE,
        );

        // GPU Interrupt (MSI vector 0x40)
        KERNEL_IDT.entries[GPU_INTERRUPT_VECTOR as usize].set_handler(
            gpu_irq as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE,
        );

        // USB Interrupt (MSI vector 0x41)
        KERNEL_IDT.entries[USB_INTERRUPT_VECTOR as usize].set_handler(
            usb_irq as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE,
        );

        // Configurar syscall handler (int 0x80)
        // Must be callable from Ring 3 (DPL 3) or it will cause #GP
        const IDT_RING_3: u8 = 0b01100000;
        KERNEL_IDT.entries[0x80].set_handler(syscall_int80 as *const () as u64, 0x08, IDT_PRESENT | IDT_RING_3 | IDT_INTERRUPT_GATE);
        
        // --- Habilitar Syscall Instruction ---
        // 1. Enable SCE (bit 0) and NXE (bit 11) in EFER.
        // NXE must be set so that the No-Execute bit (bit 63) in PTEs is valid.
        // Without NXE, any PTE written with NX=1 (e.g. by mprotect for RELRO pages)
        // causes a #PF with error code RSVD=1 when user-space accesses those pages.
        // The bootloader typically sets NXE on the BSP, but we set it explicitly here
        // to be safe and consistent with AP init.
        const EFER_SCE: u64 = 1 << 0;  // System Call Extensions
        const EFER_NXE: u64 = 1 << 11; // No-Execute Enable
        let efer = rdmsr(MSR_EFER);
        wrmsr(MSR_EFER, efer | EFER_SCE | EFER_NXE);

        // 2. Setup STAR
        // 63:48 = Sysret CS (0x08) -> Not used due to GDT layout, we use iretq
        // 47:32 = Syscall CS (0x08) -> Loads CS=0x08 (Kernel Code), SS=0x10 (Kernel Data)
        wrmsr(MSR_STAR, (0x08 << 32) | (0x08 << 48));

        // 3. Setup LSTAR (Entry point)
        wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);

        // 4. Setup SFMASK (Mask Interrupts 0x200)
        wrmsr(MSR_SFMASK, 0x200);
        
        load_idt();
    }
    
    // Deshabilitar APIC (para volver a modo Legacy PIC)
    // disable_apic();

    // Inicializar PIC
    init_pic();
    
    // Inicializar PIT (Timer)
    init_pit();

    // Habilitar teclado PS/2 (puerto principal)
    init_ps2_keyboard();

    // Habilitar ratón PS/2 (puerto auxiliar)
    init_ps2_mouse();

    // Habilitar interrupciones
    unsafe {
        asm!("sti", options(nomem, nostack));
        crate::serial::serial_print("[INT] Interrupts ENABLED\n");
    }
}

/// Initialize per-CPU interrupt infrastructure for an Application Processor (AP).
///
/// This must be called on each AP after `boot::load_gdt()` and `interrupts::load_idt()`.
/// It sets up the SYSCALL/SYSRET MSRs (EFER.SCE, STAR, LSTAR, SFMASK) which are
/// per-CPU registers and are not shared between cores.
/// Unlike `init()`, this does NOT re-initialize the PIC, PIT, PS/2 keyboard/mouse,
/// or PIT timer since those are legacy shared devices managed only by the BSP.
pub fn init_ap() {
    unsafe {
        // Enable SYSCALL/SYSRET (SCE, bit 0) and No-Execute Enable (NXE, bit 11) in EFER.
        // APs start from reset with EFER=0; the trampoline sets LME+NXE, but we set NXE
        // here too as a belt-and-suspenders measure so it's always present by the time
        // user processes run on this core.
        const EFER_SCE: u64 = 1 << 0;
        const EFER_NXE: u64 = 1 << 11;
        let efer = rdmsr(MSR_EFER);
        wrmsr(MSR_EFER, efer | EFER_SCE | EFER_NXE);

        // STAR: ring 0 CS selector (bits 47:32) and ring 3 base CS (bits 63:48).
        // This kernel uses IRETQ (not SYSRET) to return from syscalls, so the
        // bits-63:48 value is not used in practice.  Mirror the BSP's value for
        // consistency (identical to the wrmsr in init() above).
        wrmsr(MSR_STAR, (0x08 << 32) | (0x08 << 48));

        // LSTAR: entry point for the SYSCALL instruction
        wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);

        // SFMASK: clear the interrupt flag on syscall entry so the kernel stack
        // is always set up before the first timer interrupt fires on this CPU.
        wrmsr(MSR_SFMASK, 0x200);
    }
}

pub fn load_idt() {
    unsafe {
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
}


/// Inicializar teclado PS/2: habilitar IRQ de teclado (bit 0) y puerto de teclado (clear bit 4)
/// en el byte de comando del controlador PS/2. Envía "enable scanning" (0xF4) al teclado.
/// Si el controlador no responde (timeout), continúa sin bloquear el boot.
fn init_ps2_keyboard() {
    const TIMEOUT: u32 = 0x8000;
    let mut ok = true;
    unsafe {
        // Wait for PS/2 controller input buffer empty
        for i in 0..TIMEOUT {
            if inb(0x64) & 2 == 0 { break; }
            if i == TIMEOUT - 1 { ok = false; }
        }
        if !ok {
            crate::serial::serial_print("[INT] PS/2 keyboard: controller busy, skipping\n");
            return;
        }

        // Read current PS/2 controller command byte
        outb(0x64, 0x20); // Read Command Byte
        for i in 0..TIMEOUT {
            if inb(0x64) & 1 != 0 { break; } // wait for output buffer full
            if i == TIMEOUT - 1 { ok = false; }
        }
        if !ok {
            crate::serial::serial_print("[INT] PS/2 keyboard: timeout reading command byte, skipping\n");
            return;
        }
        let cmd = inb(0x60);
        // Set bit0 (keyboard IRQ1 enable), clear bit4 (keyboard port clock disable)
        let new_cmd = (cmd | 0x01) & !0x10;

        // Write modified command byte back
        for i in 0..TIMEOUT {
            if inb(0x64) & 2 == 0 { break; }
            if i == TIMEOUT - 1 { ok = false; }
        }
        if ok {
            outb(0x64, 0x60); // Write Command Byte
            for i in 0..TIMEOUT {
                if inb(0x64) & 2 == 0 { break; }
                if i == TIMEOUT - 1 { ok = false; }
            }
            if ok {
                outb(0x60, new_cmd);
            }
        }
        ok = true; // Continue even if command byte update failed

        // Send "Enable Scanning" (0xF4) directly to keyboard via port 0x60
        for i in 0..TIMEOUT {
            if inb(0x64) & 2 == 0 { break; }
            if i == TIMEOUT - 1 { ok = false; }
        }
        if !ok {
            crate::serial::serial_print("[INT] PS/2 keyboard: timeout before F4, skipping\n");
            return;
        }
        outb(0x60, 0xF4); // Enable Scanning

        // Drain ACK byte (0xFA) from output buffer so it doesn't confuse the keyboard handler
        for _i in 0..TIMEOUT {
            if inb(0x64) & 1 != 0 {
                let _ = inb(0x60); // read and discard ACK
                break;
            }
        }
    }
    crate::serial::serial_print("[INT] PS/2 keyboard init done\n");
}

/// Esperar a que el controlador PS/2 tenga el buffer de entrada libre (bit 1 de 0x64 = 0).
/// Retorna true si se liberó antes del timeout.
fn wait_ps2_write() -> bool {
    const TIMEOUT: u32 = 0x8000;
    for _ in 0..TIMEOUT {
        unsafe {
            if inb(0x64) & 2 == 0 {
                return true;
            }
        }
    }
    false
}

/// Esperar a que el controlador PS/2 tenga datos en el buffer de salida (bit 0 de 0x64 = 1).
/// Retorna true si hay datos antes del timeout.
fn wait_ps2_read() -> bool {
    const TIMEOUT: u32 = 0x8000;
    for _ in 0..TIMEOUT {
        unsafe {
            if inb(0x64) & 1 != 0 {
                return true;
            }
        }
    }
    false
}

/// Inicializar ratón PS/2: habilitar puerto auxiliar y enviar "enable data reporting".
/// Si el controlador no responde (timeout), continúa sin ratón para no colgar el boot.
fn init_ps2_mouse() {
    if !wait_ps2_write() {
        crate::serial::serial_print("[INT] PS/2: controller busy, skipping mouse init\n");
        return;
    }

    unsafe {
        // Enable mouse port
        outb(0x64, 0xA8); 
        if !wait_ps2_write() { return; }

        // Update command byte (enable KBD + MOUSE interrupts, enable both ports)
        outb(0x64, 0x20); // Read Command Byte
        if wait_ps2_read() {
            let cmd = inb(0x60);
            let new_cmd = (cmd | 0x03) & !0x30; // set bits 0,1; clear bits 4,5
            
            if wait_ps2_write() {
                outb(0x64, 0x60); // Write Command Byte
                if wait_ps2_write() {
                    outb(0x60, new_cmd);
                }
            }
        }

        // Enable data reporting for mouse
        if wait_ps2_write() {
            outb(0x64, 0xD4); // Next byte to mouse
            if wait_ps2_write() {
                outb(0x60, 0xF4); // Enable data reporting
                
                // Drain ACK
                if wait_ps2_read() {
                    let _ = inb(0x60);
                }
            }
        }
    }
    crate::serial::serial_print("[INT] PS/2 mouse/kbd ports enabled\n");
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
        
        // Habilitar timer, teclado y cascade (IRQ2) en el master
        // Slave: IRQ12 (ratón) enmascarada inicialmente para evitar IRQs antes del scheduler
        // Master: enable IRQ0, IRQ1, IRQ2 => mask 11111000 = 0xF8
        // Slave: mask all initially (0xFF); unmask_mouse_irq() tras scheduler::init()
        outb(0x21, 0xF8);
        outb(0xA1, 0xFF);
    }
}

/// Mask PIT (IRQ 0) so LAPIC timer drives the system tick on BSP.
/// Call after apic::init_timer() on BSP to avoid double ticks.
pub fn mask_pit_irq() {
    unsafe {
        let mask = inb(0x21);
        outb(0x21, mask | 1);
    }
}

/// Desenmascarar IRQ 12 (ratón PS/2). Llamar tras scheduler::init().
pub fn unmask_mouse_irq() {
    unsafe {
        outb(0xA1, 0xEF); // Slave: enable IRQ12 => mask 11101111
    }
    crate::serial::serial_print("[INT] Mouse IRQ12 enabled\n");
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
// Kernel Fault Recovery (setjmp/longjmp style)
// ============================================================================

/// Saved CPU state for fault recovery. When active, page faults and GP faults
/// will restore this state and jump back to the caller with RAX=1 (= error)
/// instead of triggering a BSOD. Clear with `clear_recovery_point()` after.
#[repr(C)]
pub struct KernelRecoveryPoint {
    /// return RIP to jump to on fault (the instruction after the `set_recovery_point` call site)
    pub rip:    u64,
    pub rsp:    u64,
    pub rbp:    u64,
    pub rbx:    u64,
    pub r12:    u64,
    pub r13:    u64,
    pub r14:    u64,
    pub r15:    u64,
    pub rflags: u64,
    /// Non-zero = recovery point is active
    pub active: u64,
}

impl KernelRecoveryPoint {
    pub const fn new() -> Self {
        Self { rip: 0, rsp: 0, rbp: 0, rbx: 0, r12: 0, r13: 0, r14: 0, r15: 0, rflags: 0, active: 0 }
    }
}

/// Per-CPU kernel recovery points – one slot per logical CPU (indexed by CPU ID).
/// Having a separate slot per core prevents concurrent kernel fault handlers on
/// different CPUs from overwriting each other's saved state.
///
/// # Safety invariant
/// Each slot `KERNEL_RECOVERY[i]` is accessed **exclusively** by the CPU whose
/// `get_cpu_id()` returns `i`.  No two CPUs share a slot, so concurrent writes
/// by different cores never alias, and per-slot accesses are data-race free.
/// The `active` field is read/written with `read_volatile`/`write_volatile` to
/// prevent the compiler from caching or eliding the memory operations.
pub static mut KERNEL_RECOVERY: [KernelRecoveryPoint; crate::boot::MAX_SMP_CPUS] =
    [const { KernelRecoveryPoint::new() }; crate::boot::MAX_SMP_CPUS];

/// Set a kernel recovery point. Returns `false` on first call (normal path).
/// If a fault fires, execution resumes here and returns `true` (error path).
///
/// # Safety
/// Must be called from kernel context (Ring 0) only.
/// The recovery point is NOT re-entrant; only one level of nesting is supported.
/// Always pair with `clear_recovery_point()` once the risky section is done.
#[inline(never)]
pub unsafe fn set_recovery_point() -> bool {
    let result: u64;
    let cpu_id = crate::process::get_cpu_id();
    // Pass the struct pointer as an `in(reg)` so the compiler emits a
    // 64-bit address materialisation (mov rXX, imm64 + lea), avoiding
    // R_X86_64_32S relocations that can't reach higher-half addresses.
    let rec_ptr: u64 = &raw mut KERNEL_RECOVERY[cpu_id] as u64;
    core::arch::asm!(
        // rax already holds rec_ptr (passed as inout)
        // Capture the resume RIP via a forward LEA to label 2:
        "lea {rip_val}, [rip + 2f]",
        "mov [{rec} + 0],  {rip_val}",  // recovery.rip
        "mov [{rec} + 8],  rsp",         // recovery.rsp
        "mov [{rec} + 16], rbp",         // recovery.rbp
        "mov [{rec} + 24], rbx",         // recovery.rbx
        "mov [{rec} + 32], r12",         // recovery.r12
        "mov [{rec} + 40], r13",         // recovery.r13
        "mov [{rec} + 48], r14",         // recovery.r14
        "mov [{rec} + 56], r15",         // recovery.r15
        "pushfq",
        "pop  {rip_val}",
        "mov [{rec} + 64], {rip_val}",  // recovery.rflags
        "mov qword ptr [{rec} + 72], 1",// recovery.active = 1
        "xor eax, eax",                 // return 0 = false (normal path)
        "2:",
        // On fault, handler restores state and jumps here with rax=1
        rec     = in(reg) rec_ptr,
        rip_val = out(reg) _,
        out("rax") result,
        options(nostack),
    );
    result != 0
}

/// Clear the active recovery point so faults revert to BSOD behaviour.
#[inline]
pub unsafe fn clear_recovery_point() {
    let cpu_id = crate::process::get_cpu_id();
    core::ptr::write_volatile(&raw mut KERNEL_RECOVERY[cpu_id].active, 0);
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

extern "C" fn exception_handler(context: &mut ExceptionContext) {
    // CR2 MUST be read before ANY operation that could trigger a nested fault
    let cr2: u64;
    let cr3: u64;
    unsafe { 
        core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)); 
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)); 
    }

    if let Some(mut stats) = INTERRUPT_STATS.try_lock() {
        stats.exceptions += 1;
    }

    let cpu_id = crate::process::get_cpu_id();
    let pid = crate::process::current_process_id().unwrap_or(0);

    let num = context.num;
    let err = context.error_code;
    let rip = context.rip;
    let rax = context.rax;
    let rbx = context.rbx;
    let rcx = context.rcx;
    let rdx = context.rdx;
    let rsi = context.rsi;
    let rdi = context.rdi;
    let rbp = context.rbp;
    let rsp = context.rsp;
    let r8  = context.r8;
    let r9  = context.r9;
    let r10 = context.r10;
    let r11 = context.r11;
    let r12 = context.r12;
    let r13 = context.r13;
    let r14 = context.r14;
    let r15 = context.r15;
    let rfl = context.rflags;
    let cs  = context.cs;
    let ss  = context.ss;

    // ---- Demand Paging: satisfy a lazy anonymous page fault ----
    // When a page-not-present fault (#PF, vector 14) arrives from userspace and the
    // faulting address falls within a lazy anonymous VMA, allocate a fresh zeroed frame
    // and map it.  The CPU will then retry the faulting instruction automatically after
    // the handler returns (via iretq in common_exception_handler).
    // This check runs BEFORE any diagnostic printing so that normal demand-paging faults
    // are handled silently without generating a noisy register dump.
    if num == 14 && pid != 0 {
        // error_code bit 0 == 0 means "not present".
        // Allow handling faults for user-space addresses (< 0xFFFF...) even if triggered by the kernel.
        if (err & 1) == 0 && cr2 < 0xFFFF_8000_0000_0000 {
            if crate::memory::handle_anon_page_fault(pid, cr2) {
                return; // Frame allocated — retry the faulting instruction.
            }
        }
    }
    // ---- End Demand Paging ----

    // CR2 is only defined for #PF (14); on other faults it is stale — do not imply a page fault.
    if num == 14 && cr2 < 4096 && pid != 0 {
        // error bit 4 = instruction fetch; RIP≈CR2 suele ser call/jmp a NULL, no un simple *NULL.
        let ifetch = (err & 0x10) != 0;
        if rip < 4096 {
            crate::serial::serial_printf(format_args!(
                "\n[PF] CR2={:#x} RIP={:#x} err={:#x}: ejecución en página cero ({}). \
Típico en compositores: puntero a función / backend Wayland o wl_* a 0 tras recurso no inicializado.\n",
                cr2, rip, err,
                if ifetch { "fetch de instrucción" } else { "acceso" },
            ));
        } else {
            crate::serial::serial_printf(format_args!(
                "\n[PF] CR2={:#x} RIP={:#x}: acceso a datos en primera página (NULL+desp); comprobar retorno de open/mmap.\n",
                cr2, rip,
            ));
        }
    }
    crate::serial::serial_printf(format_args!(
        "\n!!! EXCEPTION: {} Error: {:#018x} RIP: {:#018x} !!!\n\
         CPU: {} PID: {} Active CR3: {:#018x} CR2: {:#018x}\n\
         RAX: {:#018x} RBX: {:#018x} RCX: {:#018x} RDX: {:#018x}\n\
         RSI: {:#018x} RDI: {:#018x} RBP: {:#018x} RSP: {:#018x}\n\
         R8:  {:#018x} R9:  {:#018x} R10: {:#018x} R11: {:#018x}\n\
         R12: {:#018x} R13: {:#018x} R14: {:#018x} R15: {:#018x}\n\
         RFL: {:#018x} CS:  {:#018x} SS:  {:#018x}\n",
        num, err, rip,
        cpu_id, pid, cr3, cr2,
        rax, rbx, rcx, rdx,
        rsi, rdi, rbp, rsp,
        r8,  r9,  r10, r11,
        r12, r13, r14, r15,
        rfl, cs, ss
    ));

    // Salto a código NULL: volcar palabras en la pila de **usuario** vía tablas de páginas
    // (no leer RSP como puntero lineal del kernel: CR3 activo es del proceso).
    if num == 14 && rip == 0 && (cs & 3) == 3 && pid != 0 {
        crate::serial::serial_printf(format_args!(
            "[PF] pistas código: RBX={:#x} RCX={:#x} R13={:#x} (RCX=musl a veces RIP post-syscall)\n",
            rbx, rcx, r13
        ));
        if let Some(p) = crate::process::get_process(pid) {
            let pt = p.resources.lock().page_table_phys;
            drop(p);
            crate::serial::serial_printf(format_args!(
                "[PF] CR3_en_fault={:#x} PCB_page_table_phys={:#x}{}\n",
                cr3,
                pt,
                if cr3 != pt { " **DISCREPANCIA**" } else { "" }
            ));
            let rsp_pg = rsp & !0xFFF;
            match crate::memory::get_user_page_phys(pt, rsp_pg) {
                Some(pa) => {
                    crate::serial::serial_printf(format_args!(
                        "[PF] página RSP paddr={:#x} (va página {:#x})\n",
                        pa, rsp_pg
                    ));
                }
                None => {
                    crate::serial::serial_print("[PF] RSP: sin hoja 4K / huge en walk PTE\n");
                }
            }
            const USER_VA_MAX: u64 = 0x0000_7FFF_FFFF_FFFF;
            let dump_user_qwords = |label: &'static str, base: u64| {
                if base == 0 {
                    crate::serial::serial_printf(format_args!(
                        "[PF] volcado {}=0 omitido (NULL)\n",
                        label
                    ));
                    return;
                }
                if base > USER_VA_MAX {
                    crate::serial::serial_printf(format_args!(
                        "[PF] volcado {}={:#x} omitido (fuera VA usuario canónico)\n",
                        label, base
                    ));
                    return;
                }
                if (base & 7) != 0 {
                    if base < 0x1000 {
                        crate::serial::serial_printf(format_args!(
                            "[PF] volcado {}={:#x} omitido (valor pequeño; suele ser argc/retval syscall, no puntero)\n",
                            label, base
                        ));
                    } else {
                        crate::serial::serial_printf(format_args!(
                            "[PF] volcado {}={:#x} omitido (VA desalineada a 8)\n",
                            label, base
                        ));
                    }
                    return;
                }
                // Read the first qword first to decide the header; reuse the
                // cached value in the loop so it is fetched only once.
                let first_qword = crate::memory::try_read_user_u64(pt, base);
                if matches!(first_qword, Some(0)) {
                    crate::serial::serial_printf(format_args!(
                        "[PF] 8 qwords desde {} (va {:#x}); 1ª=0 encaja con call/jmp *({}) tras mmap/calloc:\n",
                        label, base, label
                    ));
                } else {
                    crate::serial::serial_printf(format_args!(
                        "[PF] 8 qwords desde {} (va {:#x}):\n",
                        label, base
                    ));
                }
                // Print qword 0 from the cached read, then fetch the remaining 7.
                match first_qword {
                    Some(w) => {
                        crate::serial::serial_printf(format_args!(
                            "  {}+{:3}: {:#018x}\n",
                            label, 0u64, w
                        ));
                    }
                    None => {
                        crate::serial::serial_printf(format_args!(
                            "  {}+{:3}: <no legible>\n",
                            label, 0u64
                        ));
                        return;
                    }
                }
                for i in 1..8u64 {
                    let va = base.wrapping_add(i * 8);
                    match crate::memory::try_read_user_u64(pt, va) {
                        Some(w) => {
                            crate::serial::serial_printf(format_args!(
                                "  {}+{:3}: {:#018x}\n",
                                label,
                                i * 8,
                                w
                            ));
                        }
                        None => {
                            crate::serial::serial_printf(format_args!(
                                "  {}+{:3}: <no legible>\n",
                                label,
                                i * 8
                            ));
                            break;
                        }
                    }
                }
            };
            if rbx == 0 {
                crate::serial::serial_print(
                    "[PF] RBX=0: típico call/jmp *RBX; mirar RDI/RAX si son «this» o destino de call *(%reg)\n",
                );
            } else {
                let rbx_pg = (rbx >> 12) << 12;
                match crate::memory::try_read_user_u64(pt, rbx_pg) {
                    Some(w) => {
                        crate::serial::serial_printf(format_args!(
                            "[PF] 1ª qword página RBX (va={:#x}): {:#018x}\n",
                            rbx_pg, w
                        ));
                    }
                    None => {
                        crate::serial::serial_print(
                            "[PF] página RBX no legible vía try_read_user_u64\n",
                        );
                    }
                }
                // RBX suele ser RIP previo, base de vtable o puntero a struct; el inicio de página puede ser .bss.
                if (rbx & 7) == 0 {
                    if let Some(slot) = crate::memory::try_read_user_u64(pt, rbx) {
                        crate::serial::serial_printf(format_args!(
                            "[PF] qword en dirección RBX (dato @va={:#x}): {:#018x}\n",
                            rbx, slot
                        ));
                    } else {
                        crate::serial::serial_print(
                            "[PF] qword @RBX: no legible (sin PTE hoja / phys=0 / cruce de página)\n",
                        );
                    }
                } else {
                    crate::serial::serial_printf(format_args!(
                        "[PF] RBX desalineado ({:#x}); omito lectura @RBX\n",
                        rbx
                    ));
                }
                let rbx_lo = rbx.saturating_sub(32) & !7u64;
                crate::serial::serial_print(
                    "[PF] qwords usuario alrededor RBX (RBX-32..RBX+32, alineado):\n",
                );
                for j in 0..9u64 {
                    let va = rbx_lo.wrapping_add(j * 8);
                    let rel = va as i64 - rbx as i64;
                    match crate::memory::try_read_user_u64(pt, va) {
                        Some(w) => {
                            crate::serial::serial_printf(format_args!(
                                "  RBX{:+}: va={:#x} -> {:#018x}\n",
                                rel, va, w
                            ));
                        }
                        None => {
                            crate::serial::serial_printf(format_args!(
                                "  RBX{:+}: va={:#x} -> <no legible>\n",
                                rel, va
                            ));
                        }
                    }
                }
            }
            dump_user_qwords("RDI", rdi);
            if rax != rdi {
                dump_user_qwords("RAX", rax);
            }
            if rsi != 0 && (rsi & 7) == 0 && rsi <= USER_VA_MAX && rsi != rdi {
                dump_user_qwords("RSI", rsi);
            }
            let rcx_pg = (rcx >> 12) << 12;
            match crate::memory::try_read_user_u64(pt, rcx_pg) {
                Some(w) => {
                    crate::serial::serial_printf(format_args!(
                        "[PF] 1ª qword página RCX (va={:#x}): {:#018x}\n",
                        rcx_pg, w
                    ));
                }
                None => {
                    crate::serial::serial_print("[PF] página RCX no legible vía try_read_user_u64\n");
                }
            }
            crate::serial::serial_printf(format_args!(
                "[PF] volcado pila usuario @RSP={:#x} (tabla {:#x}):\n",
                rsp, pt
            ));
            for i in 0..16u64 {
                let va = rsp.wrapping_add(i * 8);
                match crate::memory::try_read_user_u64(pt, va) {
                    Some(w) => {
                        crate::serial::serial_printf(format_args!(
                            "  RSP+{:3}: {:#018x}\n",
                            i * 8,
                            w
                        ));
                    }
                    None => {
                        crate::serial::serial_printf(format_args!(
                            "  RSP+{:3}: <no mapeado>\n",
                            i * 8
                        ));
                        break;
                    }
                }
            }
            // Con jmp *reg no hay retorno recién empujado; el marco suele tener saved RIP cerca de RBP.
            const USER_STACK_LO: u64 = 0x2000_0000;
            const USER_STACK_HI: u64 = 0x2010_0000;
            if rbp >= USER_STACK_LO && rbp < USER_STACK_HI {
                let lo = rbp.saturating_sub(72) & !7u64;
                let hi = rbp.saturating_add(24) & !7u64;
                crate::serial::serial_printf(format_args!(
                    "[PF] volcado pila usuario alrededor RBP={:#x} (RBP-72 … RBP+24):\n",
                    rbp
                ));
                let mut va = lo;
                while va <= hi {
                    match crate::memory::try_read_user_u64(pt, va) {
                        Some(w) => {
                            let rel = va as i64 - rbp as i64;
                            crate::serial::serial_printf(format_args!(
                                "  RBP{:+}: {:#018x}\n",
                                rel,
                                w
                            ));
                        }
                        None => {
                            crate::serial::serial_printf(format_args!(
                                "  {:#x}: <no mapeado>\n",
                                va
                            ));
                        }
                    }
                    va = va.wrapping_add(8);
                }
            }
        }
    }
    
    // Stack dump (first 64 bytes) — kernel RSP only.
    // Dumping a *user* RSP here is unsafe: if the RSP points to an unmapped page (e.g.
    // a stack-overflow fault) the read_volatile below generates a nested #PF inside the
    // exception handler.  A nested #PF of the same class escalates to a Double Fault,
    // losing the original exception context and making post-mortem analysis impossible.
    // Kernel RSP is always mapped (lives in kernel stacks), so the dump is safe there.
    if rsp >= 0xFFFF800000000000 {
        crate::serial::serial_printf(format_args!("  Stack Dump at {:#018x}:\n", rsp));
        unsafe {
            let stack_ptr = rsp as *const u64;
            for i in 0..8 {
                let val = core::ptr::read_volatile(stack_ptr.add(i));
                crate::serial::serial_printf(format_args!("    [+{:#02x}]: {:#018x}\n", i * 8, val));
            }
        }
    }
    
    if num == 3 { return; } // Breakpoint: return to let common_handler continue

    // ---- Fault Recovery: if a recovery point is active, longjmp back ----
    // Only intercept Page Fault (#PF=14) and General Protection (#GP=13) here,
    // since those are the faults that can arise from probing unmapped MMIO.
    if num == 14 || num == 13 {
        let active = unsafe { core::ptr::read_volatile(&raw const KERNEL_RECOVERY[cpu_id].active) };
        if active != 0 {
            crate::serial::serial_printf(format_args!(
                "[RECOVERY] Fault #{} at RIP={:#018x} CR2={:#018x} — jumping to recovery point\n",
                num, rip, cr2
            ));
            // Deactivate the recovery point *before* restoring state so a fault
            // inside the handler itself doesn't loop forever.
            unsafe { core::ptr::write_volatile(&raw mut KERNEL_RECOVERY[cpu_id].active, 0); }

            // Restore saved state and return to the saved RIP with RAX=1
            // (signals `set_recovery_point()` returned `true` = error).
            unsafe {
                let rec_ptr: u64 = &raw const KERNEL_RECOVERY[cpu_id] as u64;
                core::arch::asm!(
                    "mov rsp, [rcx +  8]",  // restore rsp
                    "mov rbp, [rcx + 16]",  // restore rbp
                    "mov rbx, [rcx + 24]",  // restore rbx
                    "mov r12, [rcx + 32]",
                    "mov r13, [rcx + 40]",
                    "mov r14, [rcx + 48]",
                    "mov r15, [rcx + 56]",
                    "push qword ptr [rcx + 64]",  // rflags
                    "popfq",
                    "mov rax, 1",                   // return value = true (fault occurred)
                    "jmp qword ptr [rcx +  0]",   // jump to saved rip
                    in("rcx") rec_ptr,
                    options(noreturn)
                );
            }
        }
    }
    // ---- End Fault Recovery ----

    // ---- Userspace fault: try signal delivery, then kill if no handler ----
    // If CS[1:0] == 3, the fault originated from ring-3 (userspace) code.
    if cs & 3 == 3 && pid != 0 {
        // Map hardware exception to POSIX signal.
        let signal_for_exc: Option<u8> = match num {
            0        => Some(8),   // #DE  → SIGFPE
            4 | 5    => Some(11),  // #OF/#BR → SIGSEGV
            6        => Some(4),   // #UD  → SIGILL
            11 | 12  => Some(11),  // #NP/#SS → SIGSEGV
            13       => Some(11),  // #GP  → SIGSEGV
            14       => Some(11),  // #PF  → SIGSEGV
            _        => None,
        };

        if let Some(signum) = signal_for_exc {
            // Try to deliver via signal frame; if the process has a handler we redirect
            // the iretq to the signal handler instead of killing the process.
            if crate::syscalls::deliver_signal_from_exception(context, pid, signum, cr2) {
                return;
            }
        }

        crate::serial::serial_printf(format_args!(
            "[FAULT] Userspace exception #{} in PID {} at RIP={:#018x} CR2={:#018x} RBX={:#018x} R11={:#018x} — killing process\n",
            num, pid, rip, cr2, rbx, r11
        ));
        crate::process::exit_process();
        crate::scheduler::schedule();
        // schedule() performs a context switch when another process is ready and does not
        // return to this point. If it does return (no other process available), we fall
        // through to BSOD to avoid resuming the now-terminated process via iretq.
    }
    // ---- End Userspace fault handling ----

    // Mostrar BSOD en pantalla
    crate::progress::bsod(&crate::progress::BsodInfo {
        exception_num: num,
        error_code:    err,
        rip, rsp, cr2, cr3,
        rax, rbx, rcx, rdx,
        rsi, rdi, rbp,
        r8, r9, r10, r11,
        r12, r13, r14, r15,
        rflags: rfl,
        cs, ss,
        cpu_id: cpu_id as u64,
        pid:    pid    as u64,
    });

    loop { 
        unsafe { asm!("hlt") } 
    }
}

#[unsafe(naked)]
unsafe extern "C" fn common_exception_handler() {
    core::arch::naked_asm!(
        // Stack has: [num, error_code, iretq_frame...]
        // iretq_frame is: [rip, cs, rflags, rsp, ss]
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 32]) for CPL (last 2 bits)
        "test qword ptr [rbp + 32], 3",
        "jz 1f",
        "swapgs",
        "1:",

        // Save GPRs
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
        
        // RSP points to r15. Offset 0.
        "mov rdi, rsp", // Argument 1: &ExceptionContext
        
        "and rsp, -16",
        "call {}",
        
        "mov rsp, rbp",
        "sub rsp, 112", // Offset of R15 (14 registers x 8 bytes)
        
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

        // Restore user GS if we are returning to userspace
        "test qword ptr [rbp + 32], 3",
        "jz 2f",
        "swapgs",
        "2:",
        
        "pop rbp",
        "add rsp, 16", // Clean up num and error_code
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

// NMI (#NMI)
#[unsafe(naked)]
unsafe extern "C" fn exception_2() {
    core::arch::naked_asm!(
        "push 0", // Dummy error code
        "push 2", // Exception num
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


extern "C" fn gpu_interrupt_handler() {
    if let Some(mut stats) = INTERRUPT_STATS.try_lock() {
        stats.irqs += 1;
    }
    // Call the NVIDIA driver's interrupt handler
    crate::nvidia::handle_interrupt();
    
    // Final de interrupción (EOI) para el Local APIC
    crate::apic::eoi();
}

extern "C" fn usb_interrupt_handler() {
    if let Some(mut stats) = INTERRUPT_STATS.try_lock() {
        stats.irqs += 1;
    }
    // Call the USB HID driver's interrupt handler
    crate::usb_hid::usb_irq_handler();
    
    // Final de interrupción (EOI) para el Local APIC
    crate::apic::eoi();
}

#[unsafe(naked)]
unsafe extern "C" fn usb_irq() {
    core::arch::naked_asm!(
        "push 0", // Dummy error code
        "push 0x41", // Vector num
        "push rbp",
        "mov rbp, rsp",
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
        "call {handler}",
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
        "add rsp, 16",
        "iretq",
        handler = sym usb_interrupt_handler,
    );
}

#[unsafe(naked)]
unsafe extern "C" fn gpu_irq() {
    core::arch::naked_asm!(
        "push 0", // Dummy error code
        "push 0x40", // Vector num
        "push rbp",
        "mov rbp, rsp",
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
        "call {handler}",
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
        "add rsp, 16",
        "iretq",
        handler = sym gpu_interrupt_handler,
    );
}

extern "C" fn timer_handler() {
    TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
    let ticks = TIMER_TICKS.load(Ordering::Relaxed);
    if let Some(mut stats) = INTERRUPT_STATS.try_lock() {
        stats.irqs += 1;
    }
    
    // Send EOI first to allow other interrupts (or this one if re-enabled) to fire
    send_eoi(0);
    
    // Poll USB HID every 2ms (antes 8ms) para no perder eventos de ratón USB
    if ticks % 2 == 0 {
        crate::usb_hid::poll();
    }

    // Procesar IPC pendiente (cola global -> mailboxes); el kernel main loop
    // ya no se ejecuta tras el primer schedule(), así que debe hacerse aquí.
    crate::ipc::process_messages();
    
    // Llamar al scheduler si está disponible
    crate::scheduler::tick();
}

#[unsafe(naked)]
unsafe extern "C" fn irq_0() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 16]) for CPL
        "test qword ptr [rbp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",

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
        // SysV AMD64 ABI: RSP must be 16-byte aligned BEFORE the CALL instruction.
        // After `and rsp, -16` (→ Y, Y%16==0) and 9 pushes (72 bytes), RSP = Y-72.
        // 72 % 16 == 8, so RSP%16==8 — WRONG.  One extra sub fixes it: Y-80, 80%16==0.
        "sub rsp, 8",
        "call {}",
        "add rsp, 8",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Restore user GS if return to userspace
        "test qword ptr [rbp + 16], 3",
        "jz 2f",
        "swapgs",
        "2:",

        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym timer_handler,
    );
}

// Keyboard Buffer (Circular)
const KEY_BUFFER_SIZE: usize = 256;
static KEY_BUFFER: Mutex<[u8; KEY_BUFFER_SIZE]> = Mutex::new([0; KEY_BUFFER_SIZE]);
static KEY_HEAD: Mutex<usize> = Mutex::new(0);
static KEY_TAIL: Mutex<usize> = Mutex::new(0);

/// Scancode translation table (US-QWERTY Set 1)
/// Index is scancode, value is (normal, shifted)
static SCANCODE_MAP: [(char, char); 128] = [
    ('\0', '\0'), ('\x1B', '\x1B'), ('1', '!'), ('2', '@'), ('3', '#'), ('4', '$'), ('5', '%'), ('6', '^'), // 0x00-0x07
    ('7', '&'), ('8', '*'), ('9', '('), ('0', ')'), ('-', '_'), ('=', '+'), ('\x08', '\x08'), ('\t', '\t'), // 0x08-0x0F
    ('q', 'Q'), ('w', 'W'), ('e', 'E'), ('r', 'R'), ('t', 'T'), ('y', 'Y'), ('u', 'U'), ('i', 'I'), // 0x10-0x17
    ('o', 'O'), ('p', 'P'), ('[', '{'), (']', '}'), ('\n', '\n'), ('\0', '\0'), ('a', 'A'), ('s', 'S'), // 0x18-0x1F
    ('d', 'D'), ('f', 'F'), ('g', 'G'), ('h', 'H'), ('j', 'J'), ('k', 'K'), ('l', 'L'), (';', ':'), // 0x20-0x27
    ('\'', '\"'), ('`', '~'), ('\0', '\0'), ('\\', '|'), ('z', 'Z'), ('x', 'X'), ('c', 'C'), ('v', 'V'), // 0x28-0x2F
    ('b', 'B'), ('n', 'N'), ('m', 'M'), (',', '<'), ('.', '>'), ('/', '?'), ('\0', '\0'), ('*', '*'), // 0x30-0x37
    ('\0', '\0'), (' ', ' '), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), // 0x38-0x3F
    ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), // 0x40-0x47
    ('\0', '\0'), ('\0', '\0'), ('-', '-'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('+', '+'), ('\0', '\0'), // 0x48-0x4F
    ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), // 0x50-0x57
    ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), // 0x58-0x5F
    ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), // 0x60-0x67
    ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), // 0x68-0x6F
    ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), // 0x70-0x77
    ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), ('\0', '\0'), // 0x78-0x7F
];

static LSHIFT_PRESSED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static RSHIFT_PRESSED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static CAPS_LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Translate scancode to ASCII, tracking modifier state.
/// Returns None if scancode is a modifier or release.
pub fn scancode_to_ascii(scancode: u8) -> Option<char> {
    match scancode {
        0x2A => { LSHIFT_PRESSED.store(true, Ordering::SeqCst); None }
        0xAA => { LSHIFT_PRESSED.store(false, Ordering::SeqCst); None }
        0x36 => { RSHIFT_PRESSED.store(true, Ordering::SeqCst); None }
        0xB6 => { RSHIFT_PRESSED.store(false, Ordering::SeqCst); None }
        0x3A => { // Caps Lock
            CAPS_LOCK.store(!CAPS_LOCK.load(Ordering::SeqCst), Ordering::SeqCst);
            None
        }
        _ if scancode < 128 => {
            let shift = LSHIFT_PRESSED.load(Ordering::SeqCst) || RSHIFT_PRESSED.load(Ordering::SeqCst);
            let caps = CAPS_LOCK.load(Ordering::SeqCst);
            let (normal, shifted) = SCANCODE_MAP[scancode as usize];
            if normal == '\0' { return None; }
            
            if normal.is_ascii_alphabetic() {
                if shift ^ caps { Some(shifted) } else { Some(normal) }
            } else {
                if shift { Some(shifted) } else { Some(normal) }
            }
        }
        _ => None // Release codes (except shift) or extended
    }
}

extern "C" fn keyboard_handler() {
    let scancode: u8;
    unsafe {
        scancode = inb(0x60);
    }
    
    // Debug: log scancode to serial
    // crate::serial::serial_print("[INT] Keyboard IRQ scancode: ");
    // crate::serial::serial_print_hex(scancode as u64);
    // crate::serial::serial_print("\n");

    // Buffer the key
    let mut head = KEY_HEAD.lock();
    let tail = KEY_TAIL.lock();
    let next_head = (*head + 1) % KEY_BUFFER_SIZE;
    
    if next_head != *tail {
        let mut buffer = KEY_BUFFER.lock();
        buffer[*head] = scancode;
        *head = next_head;
        KEY_PUSH_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    // else: buffer full, drop key
    
    drop(tail);
    drop(head);
    
    if let Some(mut stats) = INTERRUPT_STATS.try_lock() {
        stats.irqs += 1;
    }
    
    send_eoi(1);
}

/// Read a byte from the keyboard buffer (non-blocking)
/// Returns 0 if empty (since 0 is not a valid scancode generally, unless error/break)
pub fn read_key() -> u8 {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let head = KEY_HEAD.lock();
        let mut tail = KEY_TAIL.lock();
        
        if *head == *tail {
            return 0; // Buffer empty
        }
        
        let buffer = KEY_BUFFER.lock();
        let val = buffer[*tail];
        // Advance tail
        *tail = (*tail + 1) % KEY_BUFFER_SIZE;
        
        val
    })
}

// Mouse Buffer (Circular) - stores packed PS/2 packets (buttons, dx, dy)
const MOUSE_BUFFER_SIZE: usize = 128;
static MOUSE_BUFFER: Mutex<[u32; MOUSE_BUFFER_SIZE]> = Mutex::new([0; MOUSE_BUFFER_SIZE]);
static MOUSE_HEAD: Mutex<usize> = Mutex::new(0);
static MOUSE_TAIL: Mutex<usize> = Mutex::new(0);
/// Paquetes de ratón inyectados (PS/2 + USB) en los últimos 5s (debug: si p2p=0 y mouse_push=0, el kernel no recibe hardware).
pub(crate) static MOUSE_PUSH_COUNT: AtomicU64 = AtomicU64::new(0);
pub(crate) static KEY_PUSH_COUNT: AtomicU64 = AtomicU64::new(0);

// Packet assembly state (3-byte standard, 4-byte con rueda)
static MOUSE_PACKET: Mutex<[u8; 4]> = Mutex::new([0; 4]);
static MOUSE_PACKET_IDX: Mutex<usize> = Mutex::new(0);

extern "C" fn mouse_handler() {
    let b = unsafe { inb(0x60) };

    let mut idx = MOUSE_PACKET_IDX.lock();
    {
        let mut pkt = MOUSE_PACKET.lock();

        if *idx == 0 && (b & 0x08) == 0 {
            send_eoi(12);
            return;
        }

        let mut packed: u32 = 0;
        let mut do_push = false;

        // Acumular el byte recibido
        pkt[*idx] = b;
        *idx += 1;
        
        // Al completar 3 bytes, empujar inmediatamente (ratón estándar 3-byte)
        if *idx == 3 {
            let buttons = pkt[0] & 0x07;
            
            // X movement: 9-bit signed integer (Sign bit is pkt[0] bit 4)
            let mut dx = pkt[1] as i32;
            if (pkt[0] & 0x10) != 0 { dx -= 256; }
            
            // Y movement: 9-bit signed integer (Sign bit is pkt[0] bit 5)
            let mut dy = pkt[2] as i32;
            if (pkt[0] & 0x20) != 0 { dy -= 256; }
            
            // Clamp to [-128, 127] to fit in the 8-bit slice of the packed u32
            dx = dx.clamp(-128, 127);
            dy = dy.clamp(-128, 127);
            
            packed = (buttons as u32)
                | ((dx as i8 as u8 as u32) << 8)
                | ((dy as i8 as u8 as u32) << 16);
            do_push = true;
            *idx = 0;
        }

        if do_push {
            // Push into circular buffer
            let mut head = MOUSE_HEAD.lock();
            let tail = MOUSE_TAIL.lock();
            let next_head = (*head + 1) % MOUSE_BUFFER_SIZE;
            if next_head != *tail {
                let mut buf = MOUSE_BUFFER.lock();
                buf[*head] = packed;
                *head = next_head;
                MOUSE_PUSH_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            drop(tail);
            drop(head);
        }
    }

    let mut stats = INTERRUPT_STATS.try_lock();
    if let Some(mut s) = stats {
        s.irqs += 1;
    }

    send_eoi(12);
}

/// Read one packed PS/2 mouse packet from buffer (non-blocking).
/// Returns 0xFFFFFFFF if empty; otherwise packed u32: buttons | (dx as u8)<<8 | (dy as u8)<<16.
pub fn read_mouse_packet() -> u32 {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let head = MOUSE_HEAD.lock();
        let mut tail = MOUSE_TAIL.lock();

        if *head == *tail {
            return 0xFFFFFFFF;
        }

        let buf = MOUSE_BUFFER.lock();
        let val = buf[*tail];
        *tail = (*tail + 1) % MOUSE_BUFFER_SIZE;
        val
    })
}

/// Inject a scancode into the keyboard buffer (used by USB HID driver).
pub fn push_key(scancode: u8) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut head = KEY_HEAD.lock();
        let tail = KEY_TAIL.lock();
        let next_head = (*head + 1) % KEY_BUFFER_SIZE;
        if next_head != *tail {
            let mut buffer = KEY_BUFFER.lock();
            buffer[*head] = scancode;
            *head = next_head;
            KEY_PUSH_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    });
}

/// Inject a packed mouse packet into the mouse buffer (used by USB HID driver).
/// Format: buttons | (dx as u8)<<8 | (dy as u8)<<16
pub fn push_mouse_packet(packet: u32) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut head = MOUSE_HEAD.lock();
        let tail = MOUSE_TAIL.lock();
        let next_head = (*head + 1) % MOUSE_BUFFER_SIZE;
        if next_head != *tail {
            let mut buf = MOUSE_BUFFER.lock();
            buf[*head] = packet;
            *head = next_head;
            MOUSE_PUSH_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    });
}

#[unsafe(naked)]
unsafe extern "C" fn irq_1() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 16]) for CPL
        "test qword ptr [rbp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",

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
        "sub rsp, 8",
        "call {}",
        "add rsp, 8",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Restore user GS if return to userspace
        "test qword ptr [rbp + 16], 3",
        "jz 2f",
        "swapgs",
        "2:",

        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym keyboard_handler,
    );
}

#[unsafe(naked)]
unsafe extern "C" fn irq_12() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 16]) for CPL
        "test qword ptr [rbp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",

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
        "sub rsp, 8",
        "call {}",
        "add rsp, 8",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Restore user GS if return to userspace
        "test qword ptr [rbp + 16], 3",
        "jz 2f",
        "swapgs",
        "2:",

        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym mouse_handler,
    );
}

/// Obtener estadísticas de interrupciones
pub fn get_stats() -> InterruptStats {
    let stats = INTERRUPT_STATS.lock();
    InterruptStats {
        exceptions: stats.exceptions,
        irqs: stats.irqs,
    }
}

// ============================================================================
// Local APIC Timer Handler (vector 0xFF) – drives per-CPU scheduling
// ============================================================================

/// Rust-level handler for the Local APIC periodic timer interrupt.
/// On BSP: runs full system tick (TIMER_TICKS, process_messages, wake_sleeping, schedule).
/// On APs: only calls schedule(). The BSP drives the global heartbeat so sleep/wake and IPC
/// work correctly on SMP even when PIT (IRQ 0) delivery is unreliable.
extern "C" fn apic_timer_handler(cs: u64) {
    // 1. Re-trigger the timer if in non-periodic mode
    let mode = crate::apic::get_timer_mode();
    if mode == crate::apic::ApicTimerMode::OneShot {
        crate::apic::set_timer_oneshot(crate::apic::get_timer_count_1ms());
    } else if mode == crate::apic::ApicTimerMode::TSCDeadline {
        let tsc_per_ms = crate::cpu::get_tsc_frequency() * 1000;
        crate::apic::set_timer_tsc(crate::cpu::rdtsc() + tsc_per_ms);
    }

    crate::apic::eoi();

    // Use is_bsp() (derived from IA32_APIC_BASE MSR bit 8) rather than
    // `cpu_id == 0` to identify the BSP.  On some real-hardware platforms
    // the Bootstrap Processor has a non-zero LAPIC ID, so cpu_id == 0 would
    // silently skip the global heartbeat and cause TIMER_TICKS to freeze,
    // making every ticks()-based timeout spin forever.
    if crate::apic::is_bsp() {
        TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
        let ticks = TIMER_TICKS.load(Ordering::Relaxed);
        
        // Poll USB HID every 2ms
        if ticks % 2 == 0 {
            crate::usb_hid::poll();
        }

        // Update AI thermal and power model every 10ms
        if ticks % 10 == 0 {
            crate::ai_core::update_thermal_model();
        }
        
        // Process global IPC messages
        crate::ipc::process_messages();
        
        // Update global scheduler stats and wake sleeping processes
        crate::scheduler::tick();
    }

    // Per-CPU scheduling tick (handles 10ms quantum before calling schedule())
    crate::scheduler::local_tick();

    // Deliver signals for CPU-bound processes before returning to userspace.
    // Signals MUST NOT be delivered when returning to Ring 0 (kernel) as it
    // could interrupt a kernel critical section or lock acquisition.
    if (cs & 3) == 3 {
        crate::process::deliver_pending_signals_noctx();
    }
}

/// Naked trampoline for the APIC timer interrupt (saves/restores caller-saved regs).
#[unsafe(naked)]
unsafe extern "C" fn apic_timer_irq() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 16]) for CPL (last 2 bits)
        "test qword ptr [rbp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",

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
        // Pass saved CS (at [rbp + 16]) as first argument (rdi) to check for Ring 3
        "mov rdi, [rbp + 16]",
        "sub rsp, 8",
        "call {}",
        "add rsp, 8",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Restore user GS if return to userspace
        "test qword ptr [rbp + 16], 3",
        "jz 2f",
        "swapgs",
        "2:",

        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym apic_timer_handler,
    );
}

// ============================================================================
// TLB Shootdown Handler (vector 0xFD) – IPI from BSP to flush APs' TLBs
// ============================================================================

/// Rust handler for the TLB shootdown IPI.
/// Flushes the entire local TLB by reloading CR3, then acknowledges the LAPIC.
extern "C" fn tlb_shootdown_handler() {
    // Flush local TLB by reloading CR3
    unsafe {
        core::arch::asm!(
            "mov rax, cr3",
            "mov cr3, rax",
            out("rax") _,
            options(nostack, preserves_flags)
        );
    }
    crate::apic::eoi();
}

/// Naked trampoline for the TLB shootdown IPI (saves/restores caller-saved regs).
#[unsafe(naked)]
unsafe extern "C" fn tlb_shootdown_irq() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 16]) for CPL
        "test qword ptr [rbp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",

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
        "sub rsp, 8",
        "call {}",
        "add rsp, 8",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Restore user GS if return to userspace
        "test qword ptr [rbp + 16], 3",
        "jz 2f",
        "swapgs",
        "2:",

        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym tlb_shootdown_handler,
    );
}



/// Register a custom IRQ handler for the given IRQ number (0-15)
/// This allows device drivers to register their interrupt handlers dynamically
pub fn set_irq_handler(irq_num: u8, handler: fn()) -> Result<(), &'static str> {
    if irq_num >= 16 {
        return Err("IRQ number must be 0-15");
    }
    
    unsafe {
        IRQ_HANDLERS[irq_num as usize] = Some(handler);
        
        // Install the IRQ wrapper in the IDT
        let interrupt_num = 32 + irq_num;
        match irq_num {
            9 => {
                KERNEL_IDT.entries[interrupt_num as usize].set_handler(
                    irq_9 as *const () as u64,
                    0x08,
                    IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE
                );
            }
            10 => {
                KERNEL_IDT.entries[interrupt_num as usize].set_handler(
                    irq_10 as *const () as u64,
                    0x08,
                    IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE
                );
            }
            11 => {
                KERNEL_IDT.entries[interrupt_num as usize].set_handler(
                    irq_11 as *const () as u64,
                    0x08,
                    IDT_PRESENT | IDT_RING_0 | IDT_INTERRUPT_GATE
                );
            }
            _ => {
                // For other IRQs, we'd need to add wrapper functions
                return Err("IRQ wrapper not implemented for this IRQ number");
            }
        }
    }
    
    Ok(())
}

/// Generic IRQ handler that dispatches to registered handler
extern "C" fn irq_9_handler() {
    unsafe {
        if let Some(handler) = IRQ_HANDLERS[9] {
            handler();
        }
    }
    let mut stats = INTERRUPT_STATS.lock();
    stats.irqs += 1;
    drop(stats);
    send_eoi(9);
}

#[unsafe(naked)]
unsafe extern "C" fn irq_9() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 16]) for CPL
        "test qword ptr [rbp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",

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
        "sub rsp, 8",
        "call {}",
        "add rsp, 8",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Restore user GS if return to userspace
        "test qword ptr [rbp + 16], 3",
        "jz 2f",
        "swapgs",
        "2:",

        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym irq_9_handler,
    );
}

// Deleted duplicate irq_1 handlers here

extern "C" fn irq_10_handler() {
    unsafe {
        if let Some(handler) = IRQ_HANDLERS[10] {
            handler();
        }
    }
    let mut stats = INTERRUPT_STATS.lock();
    stats.irqs += 1;
    drop(stats);
    send_eoi(10);
}

#[unsafe(naked)]
unsafe extern "C" fn irq_10() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 16]) for CPL
        "test qword ptr [rbp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",

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
        "sub rsp, 8",
        "call {}",
        "add rsp, 8",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Restore user GS if return to userspace
        "test qword ptr [rbp + 16], 3",
        "jz 2f",
        "swapgs",
        "2:",

        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym irq_10_handler,
    );
}

// Deleted duplicate irq_12 handlers here

extern "C" fn irq_11_handler() {
    unsafe {
        if let Some(handler) = IRQ_HANDLERS[11] {
            handler();
        }
    }
    
    let mut stats = INTERRUPT_STATS.lock();
    stats.irqs += 1;
    drop(stats);
    
    send_eoi(11);
}

#[unsafe(naked)]
unsafe extern "C" fn irq_11() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 16]) for CPL
        "test qword ptr [rbp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",

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
        "sub rsp, 8",
        "call {}",
        "add rsp, 8",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Restore user GS if return to userspace
        "test qword ptr [rbp + 16], 3",
        "jz 2f",
        "swapgs",
        "2:",

        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym irq_11_handler,
    );
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
    arg6: u64,
    context: &mut SyscallContext,
) -> u64 {
    crate::syscalls::syscall_handler(syscall_num, arg1, arg2, arg3, arg4, arg5, arg6, context)
}

#[unsafe(naked)]
unsafe extern "C" fn syscall_int80() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 16]) for CPL
        "test qword ptr [rbp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",
        
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
        
        // Mapear argumentos desde el frame guardado (mismo layout que syscall_entry)
        // rbp-8=rax, rbp-48=rdi, rbp-40=rsi, rbp-32=rdx, rbp-72=r10, rbp-56=r8
        "mov rdi, [rbp - 8]",   // syscall_num = saved rax
        "mov rsi, [rbp - 48]",  // arg1 = saved rdi
        "mov rdx, [rbp - 40]",  // arg2 = saved rsi
        "mov rcx, [rbp - 32]",  // arg3 = saved rdx
        "mov r8, [rbp - 72]",   // arg4 = saved r10
        "mov r9, [rbp - 56]",   // arg5 = saved r8
        
        // Pasar puntero al contexto y arg6 en el stack
        "lea rax, [rbp - 112]", // Dirección de r15 (Context)
        "push rax",             // 8º argumento
        "push qword ptr [rbp - 64]", // 7º argumento (arg6 = saved r9)
        
        "call {}",
        
        "add rsp, 16", // Limpiar args en stack
        
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
        // FS and GS bases are managed by ARCH_SET_FS/GS MSRs
        
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

        // Restore user GS if return to userspace
        "test qword ptr [rsp + 8], 3", // CS is now at [rsp + 8] because rbp was popped
        "jz 2f",
        "swapgs",
        "2:",
        
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
        // RSP = User RSP (still on user stack at this point)
        
        // Switch to per-CPU kernel data via GS.
        // swapgs exchanges GS.base with IA32_KERNEL_GS_BASE, which was set
        // to &CPU_DATA[cpu_id] by load_gdt().  After swapgs:
        //   gs:[0]  = CpuData.rsp0        (kernel RSP for this CPU)
        //   gs:[8]  = CpuData.scratch_rsp (scratch save area for user RSP)
        "swapgs",

        // Save user RSP to per-CPU scratch area, then load kernel RSP.
        "mov gs:[8], rsp",
        "mov rsp, gs:[0]",

        // Build IRETQ stack frame on the kernel stack.
        // Pushes go from top-of-frame to bottom (SS pushed first, RIP last).
        // After all pushes the frame in memory from low→high is: RIP, CS, RFLAGS, RSP, SS.
        "push 0x23",                  // SS  (user data selector)
        "push qword ptr gs:[8]",      // RSP (saved user stack pointer)
        "push r11",              // RFLAGS (saved by SYSCALL in R11)
        "push 0x1B",             // CS  (user code selector)
        "push rcx",              // RIP (saved by SYSCALL in RCX)

        // NOTE: do NOT swapgs here.  The handler (syscall_handler_rust) uses
        // gs-relative reads (gs:[0]/gs:[8]/gs:[20]) for current_process_id(),
        // get_cpu_id(), and set_tss_stack().  The kernel GS must remain active
        // until the iretq at the end.

        // We push RBP first so we can use it as a reference for the Context structure
        "push rbp",
        "mov rbp, rsp",

        // Save remaining GPRs (Context Structure)
        // Order must match SyscallContext layout (which is in reverse of pushes)
        // Offset from RBP: -8 (rax), -16 (rbx) ... -112 (r15)
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
        
        // Align stack for SysV ABI
        "and rsp, -16", 
        
        // Map arguments (Linux Syscall ABI -> Rust Handler)
        // SYSCALL instruction clobbers RCX and R11, so load args from saved frame.
        // Stack layout: rbp, rax(-8), rbx(-16), rcx(-24), rdx(-32), rsi(-40), rdi(-48), r8(-56), r9(-64), r10(-72), ...
        // Rust Handler: sys_num, arg1, arg2, arg3, arg4, arg5, context
        "mov rdi, [rbp - 8]",   // syscall_num = saved rax
        "mov rsi, [rbp - 48]",  // arg1 = saved rdi (e.g. fd for read)
        "mov rdx, [rbp - 40]",  // arg2 = saved rsi (e.g. buf)
        "mov rcx, [rbp - 32]",  // arg3 = saved rdx (e.g. count)
        "mov r8, [rbp - 72]",   // arg4 = saved r10
        "mov r9, [rbp - 56]",   // arg5 = saved r8
        
        "lea rax, [rbp - 112]", // Context Ptr (address of r15)
        "push rax",             // 8th arg (context)
        "push qword ptr [rbp - 64]", // 7th arg (arg6 = saved r9)
        
        "call {handler}",
        
        "add rsp, 16", // Pop args
        "mov rsp, rbp", // Restore stack to just after RBP push
        "sub rsp, 112", // Move to start of GPRs (r15)
        
        // El resultado está en RAX. Queremos que el RAX pusheado sea este resultado
        // Offset de RAX desde RBP es -8.
        "mov [rbp - 8], rax",
        
        // CRITICAL: Restore user data segments BEFORE popping registers
        "mov ax, 0x23",  // USER_DATA_SELECTOR
        "mov ds, ax",
        "mov es, ax",
        // FS and GS must NOT be reloaded here as it would clear the Base address (MSR_FS_BASE)
        // configured by sys_arch_prctl. Since kernel doesn't change them, just leave as is.

        
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

        // Restore user GS before returning to ring 3.
        // At this point the kernel GS (CpuData) is active; swapgs swaps it back
        // to the user GS (0 for our processes) so userspace sees a clean GS.
        "swapgs",

        "iretq",
        
        handler = sym syscall_handler_rust,
    );
}

/// Trampolín para procesos hijos recién creados vía fork
/// 
/// Esta función se encarga de restaurar el contexto de usuario y saltar 
/// a Ring 3 usando los valores guardados en el PCB durante el fork.
/// Wrapper for fork_child_trampoline that clears inherited locks.
#[unsafe(naked)]
pub unsafe extern "C" fn fork_child_setup() -> ! {
    core::arch::naked_asm!(
        // Jump to trampoline for the final iretq.
        // In SMP, we don't clear locks here as they may be held by other cores.
        "jmp {trampoline}",
        trampoline = sym fork_child_trampoline,
    );
}

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
        
        // CRITICAL: Salvaguardar Kernel GS base antes de limpiar el selector GS!
        // swapgs mueve el Active GS Base a IA32_KERNEL_GS_BASE, y trae el User GS Base (0).
        // Si no hacemos esto, el mov gs, ax machacaría el Active GS (kernel) dejándolo en 0,
        // perdiendo la referencia a CpuData para el próximo syscall en este core.
        "swapgs",
        
        // NOTE: do NOT set FS here. The scheduler already restored FS_BASE (MSR 0xC0000100)
        // from proc.fs_base before entering this trampoline. Setting `mov fs, 0x23` would
        // load the GDT segment base (0) into FS.Base, wiping the TLS pointer and causing
        // TLS accesses with negative offsets to compute kernel-space addresses → #GP.
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
extern "C" fn reschedule_irq_handler() {
    crate::apic::eoi();
    crate::scheduler::schedule();
}

#[unsafe(naked)]
unsafe extern "C" fn reschedule_irq() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",

        // Check CS (at [rbp + 16]) for CPL
        "test qword ptr [rbp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",

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
        "sub rsp, 8",
        "call {}",
        "add rsp, 8",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Restore user GS if return to userspace
        "test qword ptr [rbp + 16], 3",
        "jz 2f",
        "swapgs",
        "2:",

        "mov rsp, rbp",
        "pop rbp",
        "iretq",
        sym reschedule_irq_handler,
    );
}
