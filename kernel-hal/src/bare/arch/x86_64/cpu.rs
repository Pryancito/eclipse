//! CPU information.

use raw_cpuid::CpuId;

hal_fn_impl! {
    impl mod crate::hal_fn::cpu {
        fn cpu_id() -> u8 {
            // Dense logical CPU id (0..NCPU), resolved from the sparse Local APIC
            // ID through the table populated during SMP bring-up (see `smp.rs`).
            // The raw APIC ID must NOT be used to index per-CPU arrays: it is not
            // contiguous and can exceed the CPU count, causing out-of-bounds
            // panics. `lock` owns the apic->logical map so the kernel and the lock
            // crate agree on a single id space.
            lock::current_cpu_id()
        }

        fn cpu_frequency() -> u16 {
            static CPU_FREQ_MHZ: spin::Once<u16> = spin::Once::new();
            *CPU_FREQ_MHZ.call_once(|| {
                // Fallback used when CPUID leaf 0x16 is absent (AMD, older Intel,
                // or a QEMU guest that does not pass it through).  2000 MHz is a
                // conservative estimate; real CPUs are usually faster and CPUID
                // gives the exact value when available.
                //
                // NOTE: do NOT apply .max(4000) here — that old floor made
                // timer_now() run 2–4× faster than wall-clock on CPUs < 4 GHz,
                // causing every sleep/timeout to block 2–4× longer than requested.
                // Callers that need a safety ceiling for SMP delays (smp::delay_us)
                // apply their own floor independently.
                const DEFAULT: u16 = 2000;
                CpuId::new()
                    .get_processor_frequency_info()
                    .map(|info| info.processor_base_frequency())
                    .filter(|&f| f >= 100) // reject obviously bogus readings
                    .unwrap_or(DEFAULT)
            })
        }

        fn cpu_brand() -> alloc::string::String {
            use core::arch::x86_64::__cpuid;
            let mut brand = alloc::vec::Vec::new();
            for leaf in 0x80000002..=0x80000004 {
                let res = __cpuid(leaf);
                for reg in &[res.eax, res.ebx, res.ecx, res.edx] {
                    brand.extend_from_slice(&reg.to_le_bytes());
                }
            }
            let brand_str = core::str::from_utf8(&brand)
                .unwrap_or("")
                .trim_matches('\0')
                .trim();
            alloc::string::String::from(brand_str)
        }

        fn cpu_count() -> u8 {
            super::smp::CPU_COUNT.load(core::sync::atomic::Ordering::Acquire) as u8
        }

        fn reset() -> ! {
            info!("resetting/shutting down...");
            use zcore_drivers::io::{Io, Pmio};

            // Method 1: PS/2 Controller (Keyboard Controller)
            // Writing 0xFE to port 0x64 triggers a pulse on the reset line.
            Pmio::<u8>::new(0x64).write(0xFE);

            // Method 2: PCI Reset Control Register (standard on many chipsets)
            // Port 0xCF9. 0x06 = system reset, 0x0E = hard reset.
            Pmio::<u8>::new(0xCF9).write(0x06);
            Pmio::<u8>::new(0xCF9).write(0x0E);

            // Method 3: QEMU/ACPI Poweroff (fallback for halt/poweroff)
            Pmio::<u16>::new(0x604).write(0x2000);

            // Method 4: Triple Fault (the "nuclear" option)
            // Load a zero-length IDT and trigger an interrupt.
            unsafe {
                let idtr: [u16; 5] = [0, 0, 0, 0, 0];
                core::arch::asm!("lidt [{}]", in(reg) &idtr);
                core::arch::asm!("int3");
            }

            loop {
                super::interrupt::wait_for_interrupt();
            }
        }
    }
}
