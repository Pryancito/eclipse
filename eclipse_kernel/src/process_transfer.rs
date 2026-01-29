//! Transferencia de control del kernel al userland
//!
//! Este módulo maneja la transición del kernel al userland ejecutando eclipse-systemd

extern crate alloc;

use crate::gdt::{setup_userland_gdt, GdtManager};
use crate::idt::{setup_userland_idt, IdtManager};
use crate::interrupts::manager::{initialize_interrupt_system, InterruptManager};
use crate::memory::paging::{setup_userland_paging, map_userland_memory, identity_map_userland_writable};
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

/// Constantes para direcciones de memoria
const USERLAND_CODE_MAP_SIZE: u64 = 0x200000; // 2MB para código userland
const USERLAND_STACK_RESERVE: u64 = 0x100000; // 1MB de reserva para stack
const CANONICAL_ADDR_LIMIT: u64 = 0x800000000000; // Límite de espacio de direcciones canónico inferior

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
        
        // Verificar que las direcciones estén en el espacio canónico inferior
        if context.rip >= CANONICAL_ADDR_LIMIT {
            return Err("Entry point fuera del espacio de direcciones canónico");
        }
        
        if context.rsp >= CANONICAL_ADDR_LIMIT {
            return Err("Stack pointer fuera del espacio de direcciones canónico");
        }
        
        // Configurar el entorno de ejecución del userland
        match self.setup_userland_environment() {
            Ok(pml4_addr) => {
                crate::debug::serial_write_str("PROCESS_TRANSFER: Userland environment setup successful\n");
                
                // CRITICAL FIX: Mapear el stack temporal del kernel (0x500000) en las tablas de usuario
                // Este stack se usa durante la transferencia (iretq) y DEBE estar accesible
                // después del cambio de CR3, de lo contrario ocurre un triple fault.
                const TEMP_KERNEL_STACK: u64 = 0x500000;
                const TEMP_STACK_SIZE: u64 = 0x1000; // 4KB es suficiente para el frame de iretq
                crate::debug::serial_write_str("PROCESS_TRANSFER: Mapping temporary kernel stack for CR3 switch...\n");
                identity_map_userland_writable(pml4_addr, TEMP_KERNEL_STACK, TEMP_STACK_SIZE)?;
                crate::debug::serial_write_str("PROCESS_TRANSFER: Temporary kernel stack mapped successfully\n");
                
                // Mapear el stack del userland
                let stack_base = context.rsp.saturating_sub(USERLAND_STACK_RESERVE);
                map_userland_memory(pml4_addr, stack_base, USERLAND_STACK_RESERVE + 4096)?;
                
                crate::debug::serial_write_str("PROCESS_TRANSFER: Userland stack memory mapped successfully\n");
                
                // Ejecutar el proceso
                self.execute_userland_process(context, pml4_addr)?;
                
                Ok(())
            }
            Err(e) => {
                // La configuración del entorno falló
                crate::debug::serial_write_str(&alloc::format!(
                    "PROCESS_TRANSFER: Userland environment setup failed: {}\n", e
                ));
                crate::debug::serial_write_str("PROCESS_TRANSFER: Deferring transfer - setup failed\n");
                crate::debug::serial_write_str("PROCESS_TRANSFER: System will continue with kernel loop\n");
                
                // Retornar el error para que el sistema sepa que la transferencia fue diferida
                Err("Transferencia al userland diferida: fallo en configuración del entorno")
            }
        }
    }

    /// Transferir a userland con segmentos ELF cargados
    /// 
    /// Esta función maneja la transferencia de control del kernel al userland,
    /// mapeando todos los segmentos ELF previamente cargados en memoria física
    /// al espacio de direcciones del proceso.
    /// 
    /// # Argumentos
    /// - `context`: Contexto de ejecución del proceso (registros, flags, etc.)
    /// - `loaded_process`: Proceso ELF cargado con información de segmentos
    /// 
    /// # Seguridad
    /// - Aplica el modelo W^X (Write XOR Execute) para prevenir vulnerabilidades
    /// - Segmentos ejecutables no son escribibles
    /// - Segmentos escribibles tienen NX (No-Execute) habilitado
    /// 
    /// # Retorna
    /// - `Ok(())` si la transferencia fue exitosa (no retorna en caso de éxito)
    /// - `Err(&str)` si hay un error en la configuración o mapeo
    pub fn transfer_to_userland_with_segments(
        &mut self,
        context: ProcessContext,
        loaded_process: &crate::elf_loader::LoadedProcess,
    ) -> Result<(), &'static str> {
        crate::debug::serial_write_str("PROCESS_TRANSFER: Starting userland transfer with ELF segments\n");
        crate::debug::serial_write_str(&alloc::format!(
            "PROCESS_TRANSFER: context rip=0x{:x} rsp=0x{:x}\n",
            context.rip, context.rsp
        ));
        crate::debug::serial_write_str(&alloc::format!(
            "PROCESS_TRANSFER: {} ELF segments loaded\n",
            loaded_process.segments.len()
        ));
        
        // Verificar que las direcciones estén en el espacio canónico inferior
        if context.rip >= CANONICAL_ADDR_LIMIT {
            return Err("Entry point fuera del espacio de direcciones canónico");
        }
        
        if context.rsp >= CANONICAL_ADDR_LIMIT {
            return Err("Stack pointer fuera del espacio de direcciones canónico");
        }
        
        // Configurar el entorno de ejecución del userland
        match self.setup_userland_environment() {
            Ok(pml4_addr) => {
                crate::debug::serial_write_str("PROCESS_TRANSFER: Userland environment setup successful\n");
                
                // Mapear todos los segmentos ELF cargados
                for segment in &loaded_process.segments {
                    if segment.physical_pages.is_empty() {
                        continue;
                    }
                    
                    // Determinar los flags de página según los permisos del segmento ELF
                    // Aplicar modelo W^X (Write XOR Execute) para seguridad
                    let mut flags = crate::memory::paging::PAGE_PRESENT | crate::memory::paging::PAGE_USER;
                    
                    // Verificar y aplicar permisos según flags del ELF
                    let is_writable = (segment.flags & crate::elf_loader::PF_W) != 0;
                    let is_executable = (segment.flags & crate::elf_loader::PF_X) != 0;
                    
                    // Aplicar W^X: un segmento no puede ser tanto escribible como ejecutable
                    if is_writable && is_executable {
                        crate::debug::serial_write_str(&alloc::format!(
                            "PROCESS_TRANSFER: WARNING - Segment at 0x{:x} has both W and X flags, enforcing W^X\n",
                            segment.vaddr
                        ));
                        // Dar prioridad a ejecutable sobre escribible para código
                        // Esto es más seguro que permitir escritura
                        if is_executable {
                            // Ejecutable pero no escribible
                            // No agregar PAGE_WRITABLE, no agregar PAGE_NO_EXECUTE
                        } else {
                            // Escribible pero no ejecutable
                            flags |= crate::memory::paging::PAGE_WRITABLE;
                            flags |= crate::memory::paging::PAGE_NO_EXECUTE;
                        }
                    } else {
                        // Caso normal: W^X ya se cumple
                        if is_writable {
                            flags |= crate::memory::paging::PAGE_WRITABLE;
                            flags |= crate::memory::paging::PAGE_NO_EXECUTE;
                        }
                        if !is_executable {
                            // Si no es ejecutable, marcar explícitamente como NX
                            flags |= crate::memory::paging::PAGE_NO_EXECUTE;
                        }
                        // Si es ejecutable y no escribible, solo tiene PRESENT y USER
                    }
                    
                    crate::debug::serial_write_str(&alloc::format!(
                        "PROCESS_TRANSFER: Mapping segment at vaddr=0x{:x}, {} pages, flags=0x{:x} (W={} X={})\n",
                        segment.vaddr, segment.physical_pages.len(), flags,
                        is_writable, is_executable
                    ));
                    
                    // Mapear las páginas físicas a las direcciones virtuales
                    crate::memory::paging::map_preallocated_pages(
                        pml4_addr,
                        segment.vaddr,
                        &segment.physical_pages,
                        flags,
                    )?;
                }
                
                crate::debug::serial_write_str("PROCESS_TRANSFER: All ELF segments mapped successfully\n");
                
                // CRITICAL FIX: Mapear el stack temporal del kernel (0x500000) en las tablas de usuario
                // Este stack se usa durante la transferencia (iretq) y DEBE estar accesible
                // después del cambio de CR3, de lo contrario ocurre un triple fault.
                const TEMP_KERNEL_STACK: u64 = 0x500000;
                const TEMP_STACK_SIZE: u64 = 0x1000; // 4KB es suficiente para el frame de iretq
                crate::debug::serial_write_str("PROCESS_TRANSFER: Mapping temporary kernel stack for CR3 switch...\n");
                identity_map_userland_writable(pml4_addr, TEMP_KERNEL_STACK, TEMP_STACK_SIZE)?;
                crate::debug::serial_write_str("PROCESS_TRANSFER: Temporary kernel stack mapped successfully\n");
                
                // Mapear el stack del userland
                let stack_base = context.rsp.saturating_sub(USERLAND_STACK_RESERVE);
                map_userland_memory(pml4_addr, stack_base, USERLAND_STACK_RESERVE + 4096)?;
                
                crate::debug::serial_write_str("PROCESS_TRANSFER: Userland stack memory mapped successfully\n");
                crate::debug::serial_write_str("PROCESS_TRANSFER: Ready to transfer control to userland\n");
                
                // Ejecutar el proceso
                self.execute_userland_process(context, pml4_addr)?;
                
                Ok(())
            }
            Err(e) => {
                // La configuración del entorno falló
                crate::debug::serial_write_str(&alloc::format!(
                    "PROCESS_TRANSFER: Userland environment setup failed: {}\n", e
                ));
                crate::debug::serial_write_str("PROCESS_TRANSFER: Deferring transfer - setup failed\n");
                crate::debug::serial_write_str("PROCESS_TRANSFER: System will continue with kernel loop\n");
                
                // Retornar el error para que el sistema sepa que la transferencia fue diferida
                Err("Transferencia al userland diferida: fallo en configuración del entorno")
            }
        }
    }

    /// Configurar el entorno de ejecución del userland
    fn setup_userland_environment(&self) -> Result<u64, &'static str> {
        // CRITICAL FIX: Do NOT call setup_gdt() and setup_idt() here!
        // 
        // These functions create new GDT/IDT tables on the stack, load them into
        // GDTR/IDTR registers, and then return. When the function returns, the 
        // stack-allocated GDT/IDT are freed, but the CPU is still pointing to them!
        // This causes a triple fault when any interrupt or exception occurs.
        //
        // The GDT and IDT are already properly set up during kernel initialization
        // with both kernel and userland segments, so we can safely skip these calls.
        // 
        // self.setup_gdt()?;  // REMOVED - causes triple fault
        // self.setup_idt()?;  // REMOVED - causes triple fault
        
        // Initialize interrupt system is also not needed here as it's already done
        // self.setup_interrupts()?;  // REMOVED - already initialized
        
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

    fn transfer_to_userland_with_iretq(&self, context: ProcessContext, pml4_addr: u64) -> Result<(), &'static str> {
        crate::debug::serial_write_str("PROCESS_TRANSFER: Switching CR3 and executing iretq...\n");
        crate::debug::serial_write_str(&alloc::format!(
            "PROCESS_TRANSFER: Transferring to RIP=0x{:x}, RSP=0x{:x}, PML4=0x{:x}\n",
            context.rip, context.rsp, pml4_addr
        ));
        
        let context_ptr = &context as *const ProcessContext;
        
        unsafe {
            // CRITICAL: Build the iretq stack frame AND restore registers BEFORE switching CR3
            // 
            // The temporary kernel stack at 0x500000 is identity-mapped in both
            // kernel and userland page tables to enable safe CR3 switching.
            // 
            // Process:
            // 1. Build iretq frame on temporary stack (accessible after CR3 switch)
            // 2. Restore all GPRs from context
            // 3. Switch CR3 to userland page tables
            // 4. Execute iretq to transfer to userland
            //
            // If this causes a fault (e.g., #GP, #PF), the system will triple fault
            // and reset since we don't have proper exception handlers in userland yet.
            asm!(
                // 1. Setup temporary kernel stack and build iretq frame BEFORE CR3 switch
                "mov rsp, {tmp_stack}",  
                
                // Push stack frame for iretq: SS, RSP, RFLAGS, CS, RIP
                // Offsets: SS=152, RSP=56, RFLAGS=136, CS=144, RIP=128
                "push qword ptr [rax + 152]", // SS
                "push qword ptr [rax + 56]",  // RSP
                "push qword ptr [rax + 136]", // RFLAGS
                "push qword ptr [rax + 144]", // CS
                "push qword ptr [rax + 128]", // RIP
                
                // 2. Restore ALL GPRs from context BEFORE CR3 switch
                //    This must happen before CR3 switch because the context structure
                //    is on the kernel stack which may not be mapped in userland page tables
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
                
                // Save RAX value to restore after CR3 switch
                // We need to keep using RAX temporarily to hold the new CR3 value
                "push qword ptr [rax]",      // Push context.rax to stack
                "mov rax, {new_pml4}",       // Load new PML4 address into RAX
                
                // 3. NOW switch CR3 to userland page tables
                //    ALL memory accesses are complete - no more access to kernel stack or context
                "mov cr3, rax",
                
                // 4. Restore RAX from stack (it was pushed earlier)
                "pop rax",
                
                // 5. Execute iretq to transfer to userland
                //    This pops: RIP, CS, RFLAGS, RSP, SS and jumps to userland
                //    The stack frame was built in step 1 while kernel memory was still accessible
                "iretq",
                
                in("rax") context_ptr,
                new_pml4 = in(reg) pml4_addr,
                tmp_stack = in(reg) 0x500000u64,
                options(noreturn)
            );
        }

        // This line should never be reached due to options(noreturn)
        Err("Critical: iretq failed or returned")
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
    loaded_process: &crate::elf_loader::LoadedProcess,
    stack_pointer: u64,
    argc: u64,
    argv: u64,
    envp: u64,
) -> Result<(), &'static str> {
    let mut transfer = ProcessTransfer::new();
    let mut context = ProcessContext::new(loaded_process.entry_point, stack_pointer);
    context.set_args(argc, argv, envp);
    transfer.transfer_to_userland_with_segments(context, loaded_process)?;
    Ok(())
}

pub fn simulate_eclipse_systemd_execution() -> Result<(), &'static str> {
    Ok(())
}
