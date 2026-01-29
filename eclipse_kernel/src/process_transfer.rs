//! Transferencia de control del kernel al userland
//!
//! Este módulo maneja la transición del kernel al userland ejecutando eclipse-systemd

extern crate alloc;

use crate::gdt::{setup_userland_gdt, GdtManager};
use crate::idt::{setup_userland_idt, IdtManager};
use crate::interrupts::manager::{initialize_interrupt_system, InterruptManager};
use crate::memory::paging::{setup_userland_paging, map_userland_memory, identity_map_userland_memory};
use core::arch::asm;
use core::ptr;

/// Contexto de ejecución de un proceso
#[derive(Debug, Clone)]
pub struct ProcessContext {
    pub rax: u64,    // Registro de retorno
    pub rbx: u64,    // Registro base
    pub rcx: u64,    // Contador
    pub rdx: u64,    // Datos
    pub rsi: u64,    // Índice fuente
    pub rdi: u64,    // Índice destino
    pub rbp: u64,    // Puntero base
    pub rsp: u64,    // Puntero de pila
    pub r8: u64,     // Registro 8
    pub r9: u64,     // Registro 9
    pub r10: u64,    // Registro 10
    pub r11: u64,    // Registro 11
    pub r12: u64,    // Registro 12
    pub r13: u64,    // Registro 13
    pub r14: u64,    // Registro 14
    pub r15: u64,    // Registro 15
    pub rip: u64,    // Contador de instrucciones
    pub rflags: u64, // Flags de la CPU
    pub cs: u64,     // Selector de código
    pub ss: u64,     // Selector de pila
    pub ds: u64,     // Selector de datos
    pub es: u64,     // Selector extra
    pub fs: u64,     // Selector FS
    pub gs: u64,     // Selector GS
}

impl ProcessContext {
    /// Crear contexto inicial para un proceso
    pub fn new(entry_point: u64, stack_pointer: u64) -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: stack_pointer,
            rsp: stack_pointer,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: entry_point,
            rflags: 0x202, // Interrupciones habilitadas, bit 1 reservado
            cs: 0x2B,      // Selector de código userland (GDT entry 5)
            ss: 0x23,      // Selector de pila userland (GDT entry 4)
            ds: 0x23,      // Selector de datos userland (GDT entry 4)
            es: 0x23,      // Selector extra userland (GDT entry 4)
            fs: 0x23,      // Selector FS userland (GDT entry 4)
            gs: 0x23,      // Selector GS userland (GDT entry 4)
        }
    }

    /// Configurar argumentos del proceso
    pub fn set_args(&mut self, argc: u64, argv: u64, envp: u64) {
        self.rdi = argc; // Primer argumento (argc)
        self.rsi = argv; // Segundo argumento (argv)
        self.rdx = envp; // Tercer argumento (envp)
    }
}

/// Gestor de transferencia de procesos
pub struct ProcessTransfer {
    current_pid: u32,
}

impl ProcessTransfer {
    /// Crear nuevo gestor de transferencia
    pub fn new() -> Self {
        Self { current_pid: 0 }
    }

    /// Preparar para transferir control a un proceso del userland
    pub fn transfer_to_userland(&mut self, context: ProcessContext) -> Result<(), &'static str> {
        crate::debug::serial_write_str("PROCESS_TRANSFER: Starting userland transfer sequence\n");
        crate::debug::serial_write_str(&alloc::format!(
            "PROCESS_TRANSFER: context rip=0x{:x} rsp=0x{:x}\n",
            context.rip, context.rsp
        ));
        
<<<<<<< HEAD
        // NOTA: La transferencia completa al userland requiere:
        // 1. Código ejecutable real cargado en memoria en entry_point
        // 2. Memoria mapeada correctamente (código, datos, heap, stack)
        // 3. GDT/IDT configurados para soportar cambio de privilegio
        // 
        // Sin código real en 0x400000, saltar allí causaría un #UD o #GP fault
        // que podría llevar a un triple fault si los handlers no están configurados.
        
        crate::debug::serial_write_str("PROCESS_TRANSFER: Deferring transfer - no userland code loaded yet\n");
        crate::debug::serial_write_str("PROCESS_TRANSFER: System will continue with kernel loop\n");
        
        // Retornar error indicando que la transferencia fue diferida
        Err("Transferencia al userland diferida: requiere código ejecutable cargado en memoria.")
=======
        let pml4_addr = self.setup_userland_environment()?;
        
        // Map userland code (Identity map around entry point)
        if context.rip < 0x800000000000 {
             identity_map_userland_memory(pml4_addr, context.rip & !0xFFF, 0x200000)?;
        }
        
        // Verify stack mapping if needed
        if context.rsp > 0x100000000 {
             let stack_base = context.rsp - 0x100000;
             map_userland_memory(pml4_addr, stack_base, 0x100000 + 4096)?; 
        }
        
        // Execute process
        self.execute_userland_process(context, pml4_addr)?;
        
        Ok(())
>>>>>>> ed6658f3 (correcion de bugs de compilacion en el kernel)
    }

    /// Configurar el entorno de ejecución del userland
    fn setup_userland_environment(&self) -> Result<u64, &'static str> {
        self.setup_gdt()?;
        self.setup_idt()?;
        self.setup_interrupts()?;
        let pml4_addr = setup_userland_paging()?;
        Ok(pml4_addr)
    }

    fn setup_gdt(&self) -> Result<(), &'static str> {
        setup_userland_gdt()
    }

    fn setup_idt(&self) -> Result<(), &'static str> {
        let kernel_code_selector = 0x08; 
        setup_userland_idt(kernel_code_selector)
    }

<<<<<<< HEAD
    /// Configurar paginación
    fn setup_paging(&self) -> Result<(), &'static str> {
        // Crear y configurar el gestor de paginación
        let mut paging_manager = PagingManager::new();
        let _pml4_addr = paging_manager.setup_userland_paging()?;
        
        // Cambiar a la nueva tabla de páginas
        paging_manager.switch_to_pml4();

        Ok(())
    }

    /// Configurar interrupciones
=======
>>>>>>> ed6658f3 (correcion de bugs de compilacion en el kernel)
    fn setup_interrupts(&self) -> Result<(), &'static str> {
        initialize_interrupt_system(0x08)
    }

    fn execute_userland_process(&self, context: ProcessContext, pml4_addr: u64) -> Result<(), &'static str> {
        self.setup_userland_registers(&context)?;
        self.transfer_to_userland_with_iretq(context, pml4_addr)?;
        Ok(())
    }

    fn setup_userland_registers(&self, context: &ProcessContext) -> Result<(), &'static str> {
        unsafe {
            asm!("mov ds, ax", in("ax") context.ds, options(nomem, nostack));
            asm!("mov es, ax", in("ax") context.es, options(nomem, nostack));
            asm!("mov fs, ax", in("ax") context.fs, options(nomem, nostack));
            asm!("mov gs, ax", in("ax") context.gs, options(nomem, nostack));
        }
        Ok(())
    }

<<<<<<< HEAD
    /// Transferir control al userland usando iretq
    fn transfer_to_userland_with_iretq(&self, context: ProcessContext) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&alloc::format!(
            "PROCESS_TRANSFER: Executing iretq - entry=0x{:x} stack=0x{:x}\n",
            context.rip, context.rsp
        ));
        
        unsafe {
            // Preparar el stack del kernel para iretq
            // iretq espera (de tope a fondo del stack):
            // RIP, CS, RFLAGS, RSP, SS
            
            asm!(
                // Primero guardamos el stack pointer actual en un registro temporal
                "mov r11, rsp",
                
                // Apilar valores para iretq (en orden inverso porque el stack crece hacia abajo)
                "push {ss}",              // Stack Segment (USER_DS = 0x23)
                "push {user_rsp}",        // User Stack Pointer
                "push {rflags}",          // RFLAGS
                "push {cs}",              // Code Segment (USER_CS = 0x2B)
                "push {rip}",             // Instruction Pointer
                
                // Cargar registros de propósito general del contexto
                "mov rax, {rax}",
                "mov rbx, {rbx}",
                "mov rcx, {rcx}",
                "mov rdx, {rdx}",
                "mov rsi, {rsi}",
                "mov rdi, {rdi}",
                "mov rbp, {rbp}",
                "mov r8, {r8}",
                "mov r9, {r9}",
                "mov r10, {r10}",
                // r11 se usó temporalmente, lo cargamos ahora
                "mov r11, {r11}",
                "mov r12, {r12}",
                "mov r13, {r13}",
                "mov r14, {r14}",
                "mov r15, {r15}",
                
                // Ejecutar iretq para transferir al userland (NUNCA RETORNA)
                "iretq",
                
                ss = in(reg) context.ss,
                user_rsp = in(reg) context.rsp,
                rflags = in(reg) context.rflags,
                cs = in(reg) context.cs,
                rip = in(reg) context.rip,
                rax = in(reg) context.rax,
                rbx = in(reg) context.rbx,
                rcx = in(reg) context.rcx,
                rdx = in(reg) context.rdx,
                rsi = in(reg) context.rsi,
                rdi = in(reg) context.rdi,
                rbp = in(reg) context.rbp,
                r8 = in(reg) context.r8,
                r9 = in(reg) context.r9,
                r10 = in(reg) context.r10,
                r11 = in(reg) context.r11,
                r12 = in(reg) context.r12,
                r13 = in(reg) context.r13,
                r14 = in(reg) context.r14,
                r15 = in(reg) context.r15,
                options(noreturn)
            );
        }
=======
    fn transfer_to_userland_with_iretq(&self, context: ProcessContext, pml4_addr: u64) -> Result<(), &'static str> {
        crate::debug::serial_write_str("PROCESS_TRANSFER: Switching CR3 and executing iretq...\n");
        
        let context_ptr = &context as *const ProcessContext;
        
        unsafe {
            // 1. Switch CR3
             asm!("mov cr3, {}", in(reg) pml4_addr, options(nostack));
            
            // 2. Execute iretq
            asm!(
                "mov rsp, {tmp_stack}",  
                
                // Push stack frame for iretq: SS, RSP, RFLAGS, CS, RIP
                // Offsets: SS=152, RSP=56, RFLAGS=136, CS=144, RIP=128
                "push qword ptr [rax + 152]", // SS
                "push qword ptr [rax + 56]",  // RSP
                "push qword ptr [rax + 136]", // RFLAGS
                "push qword ptr [rax + 144]", // CS
                "push qword ptr [rax + 128]", // RIP
                
                // Restore GPRs
                "mov rbx, [rax + 8]",
                "mov rcx, [rax + 16]",
                "mov rdx, [rax + 24]",
                "mov rsi, [rax + 32]",
                "mov rdi, [rax + 40]",
                "mov rbp, [rax + 48]",
                "mov r8,  [rax + 64]",
                "mov r9,  [rax + 72]",
                "mov r10, [rax + 80]",
                "mov r11, [rax + 88]",
                "mov r12, [rax + 96]",
                "mov r13, [rax + 104]",
                "mov r14, [rax + 112]",
                "mov r15, [rax + 120]",
                
                // Restore RAX last (it currently holds context_ptr)
                "mov rax, [rax]",
                
                "iretq",
                
                in("rax") context_ptr,
                tmp_stack = in(reg) 0x500000u64,
                options(noreturn)
            );
        }

        Err("Critical: iretq failed or returned")
>>>>>>> ed6658f3 (correcion de bugs de compilacion en el kernel)
    }

    pub fn get_current_pid(&self) -> u32 {
        self.current_pid
    }
}

impl Default for ProcessTransfer {
    fn default() -> Self {
        Self::new()
    }
}

pub fn transfer_to_eclipse_systemd(
    entry_point: u64,
    stack_pointer: u64,
    argc: u64,
    argv: u64,
    envp: u64,
) -> Result<(), &'static str> {
    let mut transfer = ProcessTransfer::new();
    let mut context = ProcessContext::new(entry_point, stack_pointer);
    context.set_args(argc, argv, envp);
    transfer.transfer_to_userland(context)?;
    Ok(())
}

pub fn simulate_eclipse_systemd_execution() -> Result<(), &'static str> {
    Ok(())
}
