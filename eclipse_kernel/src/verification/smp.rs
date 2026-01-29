//! SMP Stress Test

use crate::process::manager::get_process_manager;
use crate::debug::serial_write_str;
use alloc::format;

/// Worker thread function
fn worker_thread(arg: u64) {
    let id = arg;
    // We loop for a while, printing status
    let cpu_id = unsafe { crate::interrupts::apic::get_local_apic_id() };
    serial_write_str(&format!("WORKER: Thread {} started on CPU {}\n", id, cpu_id));

    // Busy wait with yields to stress scheduler
    for i in 0..50 {
        for _ in 0..100000 {
            core::hint::spin_loop();
        }
        
        let current_cpu = unsafe { crate::interrupts::apic::get_local_apic_id() };
        if i % 10 == 0 {
            serial_write_str(&format!("WORKER: Thread {} iteration {} on CPU {}\n", id, i, current_cpu));
            
            // Explicit yield? Or let timer do it?
            // Let's rely on Preemption (Timer) to test it.
            // But we can also yield occasionally.
            // crate::process::context_switch::yield_cpu(); 
        }
    }

    let end_cpu = unsafe { crate::interrupts::apic::get_local_apic_id() };
    serial_write_str(&format!("WORKER: Thread {} finished on CPU {}\n", id, end_cpu));
    
    // We must ensure we don't return from the thread function as there's nowhere to return to.
    
    // Terminate self
    {
        let mut manager_guard = get_process_manager().lock();
        if let Some(ref mut manager) = *manager_guard {
             let pid = manager.get_current_pid_safe().unwrap_or(0);
             let _ = manager.terminate_process(pid);
        }
    }
    loop { unsafe { core::arch::asm!("hlt"); } }
}

/// Start the stress test
pub fn smp_stress_test() {
    serial_write_str("TEST: Starting SMP Stress Test\n");
    
    let mut manager_guard = get_process_manager().lock();
    if let Some(ref mut manager) = *manager_guard {
             // Create 16 threads (assuming < 16 CPUs) to force queueing
             for i in 1..=16 {
                 let name = format!("worker_{}", i);
                 match manager.spawn_kernel_thread(worker_thread, i as u64, &name) {
                     Ok(pid) => serial_write_str(&format!("TEST: Spawned {} (PID {})\n", name, pid)),
                     Err(e) => serial_write_str(&format!("TEST: Failed to spawn {}: {}\n", name, e)),
                 }
             }
    } else {
        serial_write_str("TEST: ProcessManager not initialized!\n");
    }
    serial_write_str("TEST: All threads spawned. Main thread continuing...\n");
}
