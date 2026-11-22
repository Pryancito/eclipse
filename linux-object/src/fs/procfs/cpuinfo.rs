//! `/proc/cpuinfo`: one block per CPU, each ended by a blank line.

use alloc::string::String;

/// The whole file.
pub(super) fn proc_cpuinfo_content() -> String {
    let mut brand = kernel_hal::cpu::cpu_brand();
    if brand.is_empty() {
        brand = "Eclipse CPU".into();
    }
    let cpu_count = kernel_hal::cpu::cpu_count() as usize;
    // Feature flags the kernel has made usable. Missing this line made Ollama
    // (and anything that greps `flags`) guess wrong and load an AVX2/CUDA
    // runner that then died with SIGILL (`illegal instruction`).
    let flags = kernel_hal::cpu::cpuinfo_flags();
    // CPUID leaf 0, not a constant: an AMD machine said `GenuineIntel`, and
    // what keys off `vendor_id` (mesa, ollama, `lscpu`) took the wrong path.
    let mut vendor = kernel_hal::cpu::cpu_vendor();
    if vendor.is_empty() {
        vendor = "unknown".into();
    }
    let mut s = String::new();
    for i in 0..cpu_count {
        #[cfg(target_arch = "x86_64")]
        {
            use core::fmt::Write;
            // One package: every CPU has `physical id` 0, its own `core id`,
            // and `siblings` = `cpu cores` = the count. `show_cpuinfo` ends
            // each block with a blank line, which is what `lscpu` and every
            // `awk -v RS=` parser split the file on.
            let _ = writeln!(
                s,
                "processor\t: {}\n\
                 vendor_id\t: {}\n\
                 model name\t: {}\n\
                 stepping\t: 0\n\
                 cpu MHz\t\t: {:.3}\n\
                 cache size\t: 4096 KB\n\
                 physical id\t: 0\n\
                 siblings\t: {}\n\
                 core id\t\t: {}\n\
                 cpu cores\t: {}\n\
                 flags\t\t: {}\n",
                i,
                vendor,
                brand,
                kernel_hal::cpu::cpu_frequency() as f64,
                cpu_count,
                i,
                cpu_count,
                flags
            );
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            use core::fmt::Write;
            let _ = writeln!(
                s,
                "processor\t: {}\n\
                 model name\t: {}\n\
                 cpu cores\t: {}\n",
                i, brand, cpu_count
            );
            let _ = &vendor;
            let _ = flags; // unused on non-x86 until we grow a flags path
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `show_cpuinfo` ends every block with a blank line, which is what
    /// `lscpu` and `awk -v RS=` split on; one package, so `physical id` 0.
    #[test]
    fn every_cpu_is_its_own_block_ended_by_a_blank_line() {
        let s = proc_cpuinfo_content();
        let blocks: alloc::vec::Vec<&str> = s.split("\n\n").filter(|b| !b.is_empty()).collect();
        assert_eq!(blocks.len(), kernel_hal::cpu::cpu_count() as usize, "{}", s);
        assert!(s.ends_with("\n\n"), "the last block is ended too: {:?}", s);
        for (i, block) in blocks.iter().enumerate() {
            assert!(
                block.starts_with(&alloc::format!("processor\t: {i}\n")),
                "{}",
                block
            );
            #[cfg(target_arch = "x86_64")]
            {
                assert!(block.contains("\nphysical id\t: 0\n"), "{}", block);
                assert!(
                    block.contains(&alloc::format!("\ncore id\t\t: {i}\n")),
                    "{}",
                    block
                );
                assert!(!block.contains("vendor_id\t: \n"), "{}", block);
            }
        }
    }
}
