//! Driver ACPI Real para Eclipse OS
//!
//! Implementa búsqueda y parseo real de tablas ACPI (RSDP, RSDT, MADT).

use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use core::ptr;
use crate::debug::serial_write_str;
use crate::memory::paging::translate_virtual_address;

/// Firma RSDP "RSD PTR "
const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";

/// Estructura RSDP (Root System Description Pointer) Revision 1.0
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct RsdpDescriptor {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_address: u32,
}

/// Estructura RSDP Revision 2.0 (Extension)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct RsdpDescriptor20 {
    first_part: RsdpDescriptor,
    length: u32,
    xsdt_address: u64,
    extended_checksum: u8,
    reserved: [u8; 3],
}

/// Cabecera común de tablas ACPI
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct AcpiTableHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub revision: u8,
    pub checksum: u8,
    pub oem_id: [u8; 6],
    pub oem_table_id: [u8; 8],
    pub oem_revision: u32,
    pub creator_id: [u8; 4],
    pub creator_revision: u32,
}

/// Estructura MADT (Multiple APIC Description Table)
#[repr(C, packed)]
pub struct MadtHeader {
    pub header: AcpiTableHeader,
    pub local_apic_address: u32,
    pub flags: u32,
}

/// Tipos de entradas MADT
const MADT_ENTRY_PROCESSOR_LOCAL_APIC: u8 = 0;
const MADT_ENTRY_IO_APIC: u8 = 1;
const MADT_ENTRY_ISO: u8 = 2; // Interrupt Source Override

/// Cabecera de entrada MADT
#[repr(C, packed)]
struct MadtEntryHeader {
    entry_type: u8,
    record_length: u8,
}

/// Entrada Processor Local APIC
#[repr(C, packed)]
struct MadtProcessorLocalApic {
    header: MadtEntryHeader,
    acpi_processor_id: u8,
    apic_id: u8,
    flags: u32,
}

/// Gestor ACPI
pub struct AcpiManager {
    rsdp_address: u64,
    rsdt_address: u64,
    xsdt_address: u64,
    madt_address: u64,
    cpu_count: AtomicU32,
    is_initialized: AtomicBool,
    // Almacenamos los APIC IDs de los procesadores encontrados (max 255)
    pub detected_apic_ids: [u8; 256],
}

impl AcpiManager {
    pub const fn new() -> Self {
        Self {
            rsdp_address: 0,
            rsdt_address: 0,
            xsdt_address: 0,
            madt_address: 0,
            cpu_count: AtomicU32::new(0),
            is_initialized: AtomicBool::new(false),
            detected_apic_ids: [0xFF; 256], // 0xFF = slot vacío (aunque 0xFF es un ID broadcast valido, lo usaremos como marcador por ahora: en boot IDs suelen ser pequeños)
        }
    }

    /// Inicializar el subsistema ACPI
    pub fn init(&mut self) -> Result<u32, &'static str> {
        if self.is_initialized.load(Ordering::Acquire) {
            return Ok(self.cpu_count.load(Ordering::Relaxed));
        }

        serial_write_str("ACPI: Buscando RSDP...\n");
        let rsdp_addr = self.find_rsdp().ok_or("ACPI: RSDP no encontrado")?;
        self.rsdp_address = rsdp_addr;
        
        crate::debug::serial_write_str("ACPI: RSDP encontrado en 0x");
        crate::memory::paging::print_hex(rsdp_addr);
        crate::debug::serial_write_str("\n");

        // Validar y obtener RSDT/XSDT
        unsafe {
            let rsdp = &*(rsdp_addr as *const RsdpDescriptor);
            if rsdp.revision >= 2 {
                 let rsdp2 = &*(rsdp_addr as *const RsdpDescriptor20);
                 self.xsdt_address = rsdp2.xsdt_address;
                 serial_write_str("ACPI: Usando XSDT (RSDP v2.0+)\n");
            } else {
                 self.rsdt_address = rsdp.rsdt_address as u64;
                 serial_write_str("ACPI: Usando RSDT (RSDP v1.0)\n");
            }
        }

        // Buscar tabla MADT (APIC)
        self.madt_address = self.find_table(b"APIC").ok_or("ACPI: MADT (APIC) no encontrada")?;
        
        crate::debug::serial_write_str("ACPI: MADT encontrada en 0x");
        crate::memory::paging::print_hex(self.madt_address);
        crate::debug::serial_write_str("\n");

        // Parsear MADT
        self.parse_madt()?;

        self.is_initialized.store(true, Ordering::Release);
        Ok(self.cpu_count.load(Ordering::Relaxed))
    }

    /// Buscar el puntero RSDP en la memoria BIOS
    fn find_rsdp(&self) -> Option<u64> {
        // EBDA (Extended BIOS Data Area)
        // Normalmente buscamos en 0xE0000 - 0xFFFFF
        // TODO: Buscar también en la primera KB del EBDA si está definido
        
        // Búsqueda simplificada: 0xE0000 -> 0xFFFFF en saltos de 16 bytes
        let start = 0xE0000;
        let end = 0xFFFFF;
        
        for addr in (start..end).step_by(16) {
            unsafe {
                // Checkpoint every 16KB to avoid spam
                if addr % 16384 == 0 {
                    serial_write_str("ACPI: Scanning 0x");
                    crate::memory::paging::print_hex(addr as u64);
                    serial_write_str("\n");
                }

                let ptr = addr as *const u8;
                // Verificar firma "RSD PTR "
                let mut match_sig = true;
                for i in 0..8 {
                    if *ptr.add(i) != RSDP_SIGNATURE[i] {
                        match_sig = false;
                        break;
                    }
                }
                
                if match_sig {
                    serial_write_str("ACPI: Signature match at 0x");
                    crate::memory::paging::print_hex(addr as u64);
                    serial_write_str("\n");
                    
                    // Verificar checksum
                    if self.validate_checksum(ptr, 20) {
                         serial_write_str("ACPI: Checksum OK\n");
                        return Some(addr as u64);
                    } else {
                        serial_write_str("ACPI: Checksum FAIL\n");
                    }
                }
            }
        }
        
        None
    }

    /// Validar checksum ACPI (la suma de bytes debe ser 0)
    fn validate_checksum(&self, ptr: *const u8, mut length: usize) -> bool {
        let mut sum: u8 = 0;
        unsafe {
            for i in 0..length {
                sum = sum.wrapping_add(*ptr.add(i));
            }
        }
        sum == 0
    }

    /// Buscar una tabla específica por firma
    pub fn find_table(&self, signature: &[u8; 4]) -> Option<u64> {
        // Usar XSDT si está disponible, sino RSDT
        if self.xsdt_address != 0 {
            self.scan_xsdt(signature)
        } else if self.rsdt_address != 0 {
            self.scan_rsdt(signature)
        } else {
            None
        }
    }

    fn scan_rsdt(&self, signature: &[u8; 4]) -> Option<u64> {
        unsafe {
            let header = &*(self.rsdt_address as *const AcpiTableHeader);
            if !self.validate_checksum(header as *const _ as *const u8, header.length as usize) {
                return None;
            }
            
            // Los datos empiezan después del header
            let entries_start = (self.rsdt_address + core::mem::size_of::<AcpiTableHeader>() as u64) as *const u32;
            let entry_count = (header.length as usize - core::mem::size_of::<AcpiTableHeader>()) / 4;
            
            for i in 0..entry_count {
                let table_addr = *entries_start.add(i) as u64;
                if self.check_table_signature(table_addr, signature) {
                    return Some(table_addr);
                }
            }
        }
        None
    }

    fn scan_xsdt(&self, signature: &[u8; 4]) -> Option<u64> {
        unsafe {
            let header = &*(self.xsdt_address as *const AcpiTableHeader);
             if !self.validate_checksum(header as *const _ as *const u8, header.length as usize) {
                return None;
            }
            
            // XSDT usa direcciones de 64 bits
            let entries_start = (self.xsdt_address + core::mem::size_of::<AcpiTableHeader>() as u64) as *const u64;
            let entry_count = (header.length as usize - core::mem::size_of::<AcpiTableHeader>()) / 8;
            
            for i in 0..entry_count {
                let table_addr = *entries_start.add(i);
                 if self.check_table_signature(table_addr, signature) {
                    return Some(table_addr);
                }
            }
        }
        None
    }

    /// Verificar si una tabla tiene la firma buscada
    unsafe fn check_table_signature(&self, table_addr: u64, signature: &[u8; 4]) -> bool {
        let header = &*(table_addr as *const AcpiTableHeader);
        header.signature == *signature
    }

    /// Parsear MADT para encontrar CPUs
    fn parse_madt(&mut self) -> Result<(), &'static str> {
        unsafe {
            let madt = &*(self.madt_address as *const MadtHeader);
            
             // El tamaño de la estructura MADT en memoria incluye el header estándar + campos MADT
            // Pero MadtHeader struct solo tiene campos fijos.
            // Los entries empiezan después de: AcpiTableHeader (36) + local_apic_addr (4) + flags (4) = 44 bytes
            let mut ptr = (self.madt_address + core::mem::size_of::<MadtHeader>() as u64) as *const u8;
            let end_ptr = (self.madt_address + madt.header.length as u64) as *const u8;
            
            let mut count = 0;
            // Limpiar IDs
            for i in 0..256 { self.detected_apic_ids[i] = 0xFF; }

            // Iterar sobre las entradas de longitud variable
            while ptr < end_ptr {
                let header = &*(ptr as *const MadtEntryHeader);
                
                if header.entry_type == MADT_ENTRY_PROCESSOR_LOCAL_APIC {
                    let entry = &*(ptr as *const MadtProcessorLocalApic);
                    // Bit 0 de flags indica si el procesador está habilitado
                    if (entry.flags & 1) != 0 {
                        serial_write_str("ACPI: Found CPU Core. APIC ID: ");
                        crate::memory::paging::print_hex(entry.apic_id as u64);
                        serial_write_str("\n");
                        
                        if count < 256 {
                            self.detected_apic_ids[count] = entry.apic_id;
                            count += 1;
                        }
                    }
                }

                if header.record_length == 0 { break; } // Evitar bucle infinito si hay corrupción
                ptr = ptr.add(header.record_length as usize);
            }
            
            self.cpu_count.store(count as u32, Ordering::Release);
        }
        Ok(())
    }
}

/// Instancia global
static mut ACPI_MANAGER: Option<AcpiManager> = None;

pub fn init_acpi() -> Result<u32, &'static str> {
    let mut manager = AcpiManager::new();
    let count = manager.init()?;
    
    unsafe {
        ACPI_MANAGER = Some(manager);
    }
    Ok(count)
}

pub fn get_acpi_manager() -> Option<&'static mut AcpiManager> {
    unsafe { ACPI_MANAGER.as_mut() }
}

/// Procesar eventos ACPI (Stub)
pub fn process_acpi_events() -> Result<(), u32> {
    Ok(())
}

/// Shutdown ACPI (Stub)
pub fn shutdown_acpi() -> Result<(), &'static str> {
    Ok(())
}
