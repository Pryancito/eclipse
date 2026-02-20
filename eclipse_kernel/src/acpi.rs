//! Minimal ACPI parser for CPU and APIC discovery

use crate::serial::serial_printf;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Rsdp {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_addr: u32,
    length: u32,
    xsdt_addr: u64,
    ext_checksum: u8,
    reserved: [u8; 3],
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct SdtHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub revision: u8,
    pub checksum: u8,
    pub oem_id: [u8; 6],
    pub oem_table_id: [u8; 8],
    pub oem_revision: u32,
    pub creator_id: u32,
    pub creator_revision: u32,
}

#[repr(C, packed)]
pub struct Madt {
    pub header: SdtHeader,
    pub local_apic_address: u32,
    pub flags: u32,
}

#[repr(C, packed)]
pub struct MadtEntryHeader {
    pub entry_type: u8,
    pub entry_length: u8,
}

pub struct AcpiInfo {
    pub lapic_addr: u64,
    pub cpu_count: usize,
    pub ioapic_addr: u64,
}

static mut ACPI_INFO: Option<AcpiInfo> = None;

pub fn init(rsdp_phys: u64) {
    if rsdp_phys == 0 {
        serial_printf(format_args!("[ACPI] Error: RSDP physical address is null\n"));
        return;
    }

    unsafe {
        use crate::memory::phys_to_virt;
        
        let mut current_phys = rsdp_phys;
        let mut rsdp_virt = phys_to_virt(current_phys);

        serial_printf(format_args!("[ACPI] Searching for RSDP near physical {:#x} (virt {:#x})\n", current_phys, rsdp_virt));

        // Check if signature is at the provided address, or search nearby (UEFI sometimes points to weird offsets?)
        let mut found = false;
        let mut rsdp_ptr = rsdp_virt as *const Rsdp;
        
        if &(*rsdp_ptr).signature == b"RSD PTR " {
            found = true;
        } else {
            // Try searching from the start of the 16-byte boundary (RSDP must be 16-byte aligned)
            let aligned_phys = rsdp_phys & !0xF;
            for offset in (0..64).step_by(16) {
                let test_phys = aligned_phys.saturating_sub(64).saturating_add(offset);
                let test_virt = phys_to_virt(test_phys);
                let test_ptr = test_virt as *const Rsdp;
                if &(*test_ptr).signature == b"RSD PTR " {
                    current_phys = test_phys;
                    rsdp_virt = test_virt;
                    rsdp_ptr = test_ptr;
                    found = true;
                    serial_printf(format_args!("[ACPI] Found RSDP signature at redirected physical {:#x}\n", current_phys));
                    break;
                }
            }
        }

        if !found {
            serial_printf(format_args!("[ACPI] Error: RSDP signature 'RSD PTR ' not found at {:#x}\n", rsdp_phys));
            // Dump context to see what's there
            serial_printf(format_args!("[ACPI] Raw dump at {:#x}:\n", rsdp_virt));
            let ptr = rsdp_virt as *const u8;
            for i in 0..64 {
                crate::serial::serial_print_hex(*ptr.add(i) as u64);
                crate::serial::serial_print(" ");
                if (i + 1) % 16 == 0 { crate::serial::serial_print("\n"); }
            }
            return;
        }

        let rsdp = &*rsdp_ptr;
        let signature = rsdp.signature;
        let revision = rsdp.revision;
        let rsdt_addr = rsdp.rsdt_addr;
        let xsdt_addr = rsdp.xsdt_addr;

        serial_printf(format_args!("[ACPI] RSDP revision {} found at phys {:#x}\n", revision, current_phys));
        serial_printf(format_args!("[ACPI]   RSDT phys: {:#x}\n", rsdt_addr));
        serial_printf(format_args!("[ACPI]   XSDT phys: {:#x}\n", xsdt_addr));

        let sdt_phys = if revision >= 2 && xsdt_addr != 0 {
            xsdt_addr
        } else {
            rsdt_addr as u64
        };

        if sdt_phys == 0 {
            serial_printf(format_args!("[ACPI] Error: No RSDT/XSDT address in RSDP\n"));
            return;
        }

        let header_virt = phys_to_virt(sdt_phys);
        let header = &*(header_virt as *const SdtHeader);
        let signature = header.signature;
        let length = header.length;

        let is_xsdt = &signature == b"XSDT";
        
        serial_printf(format_args!("[ACPI] Using {} at phys {:#x}\n", if is_xsdt { "XSDT" } else { "RSDT" }, sdt_phys));

        let entry_count = (length as usize - core::mem::size_of::<SdtHeader>()) / if is_xsdt { 8 } else { 4 };
        let entries_ptr = (header_virt + core::mem::size_of::<SdtHeader>() as u64) as *const u8;

        let mut lapic_phys: u64 = 0;
        let mut ioapic_phys: u64 = 0;
        let mut cpu_count = 0;

        for i in 0..entry_count {
            let table_phys = if is_xsdt {
                *(entries_ptr.add(i * 8) as *const u64)
            } else {
                *(entries_ptr.add(i * 4) as *const u32) as u64
            };

            let table_header = &*(phys_to_virt(table_phys) as *const SdtHeader);
            if &table_header.signature == b"APIC" {
                serial_printf(format_args!("[ACPI] Found MADT (APIC) table at phys {:#x}\n", table_phys));
                let madt = &*(phys_to_virt(table_phys) as *const Madt);
                let lapic_addr_val = madt.local_apic_address;
                lapic_phys = lapic_addr_val as u64;

                // Parse MADT entries
                let madt_len = madt.header.length;
                let mut current_entry = (phys_to_virt(table_phys) + core::mem::size_of::<Madt>() as u64) as *const u8;
                let madt_end = phys_to_virt(table_phys) + madt_len as u64;

                while (current_entry as u64) < madt_end {
                    let entry_header = &*(current_entry as *const MadtEntryHeader);
                    let e_type = entry_header.entry_type;
                    let e_len = entry_header.entry_length;

                    match e_type {
                        0 => { // Processor Local APIC
                            let processor_id = *current_entry.add(2);
                            let apic_id = *current_entry.add(3);
                            let flags = *(current_entry.add(4) as *const u32);
                            if flags & 1 != 0 { // Enabled
                                cpu_count += 1;
                                serial_printf(format_args!("[ACPI]   - CPU {} (APIC ID {})\n", processor_id, apic_id));
                            }
                        }
                        1 => { // I/O APIC
                            let ioapic_id = *current_entry.add(2);
                            let addr = *(current_entry.add(4) as *const u32);
                            ioapic_phys = addr as u64;
                            serial_printf(format_args!("[ACPI]   - I/O APIC ID {} at {:#x}\n", ioapic_id, ioapic_phys));
                        }
                        _ => {}
                    }
                    current_entry = current_entry.add(e_len as usize);
                }
            }
        }

        ACPI_INFO = Some(AcpiInfo {
            lapic_addr: lapic_phys,
            cpu_count,
            ioapic_addr: ioapic_phys,
        });

        serial_printf(format_args!("[ACPI] Discovery complete: {} CPUs, LAPIC={:#x}, IOAPIC={:#x}\n", 
            cpu_count, lapic_phys, ioapic_phys));
    }
}

pub fn get_info() -> &'static AcpiInfo {
    unsafe { ACPI_INFO.as_ref().expect("ACPI not initialized") }
}
