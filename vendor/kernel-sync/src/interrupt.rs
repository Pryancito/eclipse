use core::cell::{RefCell, RefMut};

cfg_if::cfg_if! {
    if #[cfg(all(target_os = "none", any(target_arch = "riscv32", target_arch = "riscv64")))] {
        mod interrupts {
            use core::sync::atomic::{AtomicU8, Ordering};
            use riscv::register::sstatus;

            /// Maps a hardware hart id (in `tp`, possibly sparse — e.g. boards that
            /// reserve hart 0) to a dense logical CPU id (0..NCPU). Populated by the
            /// HAL during SMP bring-up via [`set_logical_cpu_id`]; reads 0 until then
            /// (correct, since only the boot hart = logical 0 runs that early).
            static HARTID_TO_LOGICAL: [AtomicU8; 256] = {
                const ZERO: AtomicU8 = AtomicU8::new(0);
                [ZERO; 256]
            };

            /// Raw hart id of the current CPU (kernel convention: stored in `tp`).
            fn raw_hart_id() -> u8 {
                let hart_id: usize;
                unsafe {
                    core::arch::asm!("mv {0}, tp", out(reg) hart_id);
                }
                hart_id as u8
            }

            /// Register the logical id assigned to a given hart id.
            pub fn set_logical_cpu_id(hart_id: u8, logical_id: u8) {
                HARTID_TO_LOGICAL[hart_id as usize].store(logical_id, Ordering::Release);
            }

            pub(crate) fn cpu_id() -> u8 {
                HARTID_TO_LOGICAL[raw_hart_id() as usize].load(Ordering::Acquire)
            }
            pub(crate) fn intr_on() {
                unsafe { sstatus::set_sie() };
            }
            pub(crate) fn intr_off() {
                unsafe { sstatus::clear_sie() };
            }
            pub(crate) fn intr_get() -> bool {
                sstatus::read().sie()
            }
        }
    } else if #[cfg(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64")))] {
        mod interrupts {
            use core::sync::atomic::{AtomicU8, Ordering};
            use x86_64::instructions::interrupts;

            /// Maps a hardware Local APIC ID (sparse, 0..=255) to a dense logical
            /// CPU id (0..NCPU). APIC IDs are *not* contiguous on real hardware
            /// (cores/threads/sockets leave gaps), so using them directly to index
            /// per-CPU arrays causes out-of-bounds panics. The table is populated by
            /// the HAL during SMP bring-up via [`set_logical_cpu_id`]. Until then it
            /// reads 0, which is correct because only the BSP (logical 0) runs before
            /// the APs are enumerated.
            static APIC_TO_LOGICAL: [AtomicU8; 256] = {
                const ZERO: AtomicU8 = AtomicU8::new(0);
                [ZERO; 256]
            };

            /// Raw initial Local APIC ID of the current CPU.
            fn raw_apic_id() -> u8 {
                raw_cpuid::CpuId::new()
                    .get_feature_info()
                    .unwrap()
                    .initial_local_apic_id() as u8
            }

            /// Register the logical id assigned to a given Local APIC ID. Called once
            /// per CPU from the HAL before that CPU starts executing kernel code.
            pub fn set_logical_cpu_id(apic_id: u8, logical_id: u8) {
                APIC_TO_LOGICAL[apic_id as usize].store(logical_id, Ordering::Release);
            }

            pub(crate) fn cpu_id() -> u8 {
                APIC_TO_LOGICAL[raw_apic_id() as usize].load(Ordering::Acquire)
            }
            pub(crate) fn intr_on() {
                interrupts::enable();
            }
            pub(crate) fn intr_off() {
                interrupts::disable();
            }
            pub(crate) fn intr_get() -> bool {
                interrupts::are_enabled()
            }
        }
    } else if #[cfg(all(target_os = "none", target_arch = "aarch64"))] {
        mod interrupts {
            pub(crate) fn cpu_id() -> u8 {
                // Dense logical id, written to TPIDR_EL1 by the kernel per CPU.
                // MPIDR affinity is sparse across clusters (Aff0 repeats), so it
                // can't index per-CPU arrays; TPIDR_EL1 holds the logical id
                // directly (0 on the boot CPU until secondaries are brought up).
                let id: u64;
                unsafe { core::arch::asm!("mrs {0}, tpidr_el1", out(reg) id) };
                id as u8
            }
            pub(crate) fn intr_on() {
                unsafe {
                    core::arch::asm!("msr daifclr, #2");
                }
            }
            pub(crate) fn intr_off() {
                unsafe {
                    core::arch::asm!("msr daifset, #2");
                }
            }
            pub(crate) fn intr_get() -> bool {
                use cortex_a::registers::DAIF;
                use tock_registers::interfaces::Readable;
                !DAIF.is_set(DAIF::I)
            }
        }
    } else {
        mod interrupts {
            pub(crate) fn cpu_id() -> u8 {
                unimplemented!();
            }
            pub(crate) fn intr_on() { unimplemented!(); }
            pub(crate) fn intr_off() { unimplemented!(); }
            pub(crate) fn intr_get() -> bool {
                unimplemented!();
            }
        }
    }
}

use interrupts::*;

/// Current CPU's dense logical id (0..NCPU).
///
/// On x86 this resolves the sparse Local APIC ID through the table populated by
/// [`set_logical_cpu_id`]; on riscv/aarch64 the architecture already provides a
/// dense id (hart id / MPIDR affinity).
pub fn current_cpu_id() -> u8 {
    cpu_id()
}

/// Register the dense logical id assigned to a hardware CPU id (Local APIC ID on
/// x86, hart id on riscv).
///
/// Must be called once per CPU (including the BSP) before that CPU executes any
/// code that takes a lock, so that `cpu_id()` never returns a stale/colliding id.
#[cfg(all(
    target_os = "none",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "riscv32",
        target_arch = "riscv64"
    )
))]
pub fn set_logical_cpu_id(hw_id: u8, logical_id: u8) {
    interrupts::set_logical_cpu_id(hw_id, logical_id)
}

#[derive(Debug, Default, Clone, Copy)]
#[repr(align(64))]
pub struct Cpu {
    pub noff: i32,              // Depth of push_off() nesting.
    pub interrupt_enable: bool, // Were interrupts enabled before push_off()?
}

impl Cpu {
    const fn new() -> Self {
        Self {
            noff: 0,
            interrupt_enable: false,
        }
    }
}

pub struct SafeRefCell<T>(RefCell<T>);

// #Safety: Only the corresponding cpu will access it.
unsafe impl<Cpu> Sync for SafeRefCell<Cpu> {}

impl<T> SafeRefCell<T> {
    const fn new(t: T) -> Self {
        Self(RefCell::new(t))
    }
}

// Avoid hard code
#[allow(clippy::declare_interior_mutable_const)]
const DEFAULT_CPU: SafeRefCell<Cpu> = SafeRefCell::new(Cpu::new());

const MAX_CORE_NUM: usize = 16;

static CPUS: [SafeRefCell<Cpu>; MAX_CORE_NUM] = [DEFAULT_CPU; MAX_CORE_NUM];

pub fn mycpu() -> RefMut<'static, Cpu> {
    CPUS[cpu_id() as usize].0.borrow_mut()
}

// push_off/pop_off are like intr_off()/intr_on() except that they are matched:
// it takes two pop_off()s to undo two push_off()s.  Also, if interrupts
// are initially off, then push_off, pop_off leaves them off.
pub(crate) fn push_off() {
    let old = intr_get();
    intr_off();
    let mut cpu = mycpu();
    if cpu.noff == 0 {
        cpu.interrupt_enable = old;
    }
    cpu.noff += 1;
}

pub(crate) fn pop_off() {
    let mut cpu = mycpu();
    if intr_get() || cpu.noff < 1 {
        panic!("pop_off");
    }
    cpu.noff -= 1;
    let should_enable = cpu.noff == 0 && cpu.interrupt_enable;
    drop(cpu);
    // NOTICE: intr_on() may lead to an immediate inerrupt, so we *MUST* drop(cpu) in advance.
    if should_enable {
        intr_on();
    }
}
