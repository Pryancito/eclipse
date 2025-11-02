//! Acceso global seguro al controlador XHCI
//! 
//! Proporciona acceso thread-safe al controlador XHCI para polling
//! de eventos sin causar deadlocks.

use spin::Mutex;
use crate::drivers::usb_xhci_improved::ImprovedXhciController;

/// Controlador XHCI global (None hasta que se inicialice)
static GLOBAL_XHCI: Mutex<Option<u64>> = Mutex::new(None);

/// Guarda la dirección MMIO del controlador XHCI global
/// 
/// Debe llamarse después de inicializar el XHCI en main_simple.rs
pub fn set_xhci_mmio_base(mmio_base: u64) {
    *GLOBAL_XHCI.lock() = Some(mmio_base);
}

/// Obtiene la dirección MMIO del controlador XHCI
pub fn get_xhci_mmio_base() -> Option<u64> {
    *GLOBAL_XHCI.lock()
}

/// Lee un registro de 32 bits del XHCI de forma segura
#[inline]
pub fn xhci_read32(offset: u64) -> Option<u32> {
    if let Some(base) = get_xhci_mmio_base() {
        let addr = (base + offset) as *const u32;
        Some(unsafe { core::ptr::read_volatile(addr) })
    } else {
        None
    }
}

/// Escribe un registro de 32 bits del XHCI de forma segura
#[inline]
pub fn xhci_write32(offset: u64, value: u32) -> Result<(), &'static str> {
    if let Some(base) = get_xhci_mmio_base() {
        let addr = (base + offset) as *mut u32;
        unsafe { core::ptr::write_volatile(addr, value); }
        Ok(())
    } else {
        Err("XHCI no inicializado")
    }
}

/// Lee un registro de 64 bits del XHCI de forma segura
#[inline]
pub fn xhci_read64(offset: u64) -> Option<u64> {
    if let Some(base) = get_xhci_mmio_base() {
        let addr = (base + offset) as *const u64;
        Some(unsafe { core::ptr::read_volatile(addr) })
    } else {
        None
    }
}

/// Escribe un registro de 64 bits del XHCI de forma segura
#[inline]
pub fn xhci_write64(offset: u64, value: u64) -> Result<(), &'static str> {
    if let Some(base) = get_xhci_mmio_base() {
        let addr = (base + offset) as *mut u64;
        unsafe { core::ptr::write_volatile(addr, value); }
        Ok(())
    } else {
        Err("XHCI no inicializado")
    }
}

/// Offsets de registros XHCI importantes
pub mod offsets {
    // Capability Registers
    pub const CAPLENGTH: u64 = 0x00;
    pub const HCSPARAMS1: u64 = 0x04;
    pub const HCSPARAMS2: u64 = 0x08;
    pub const HCCPARAMS1: u64 = 0x10;
    pub const DBOFF: u64 = 0x14;
    pub const RTSOFF: u64 = 0x18;
    
    // Operational Registers (base + CAPLENGTH)
    pub const OP_USBCMD: u64 = 0x00;
    pub const OP_USBSTS: u64 = 0x04;
    pub const OP_PAGESIZE: u64 = 0x08;
    pub const OP_DNCTRL: u64 = 0x14;
    pub const OP_CRCR: u64 = 0x18;      // Command Ring Control Register
    pub const OP_DCBAAP: u64 = 0x30;    // Device Context Base Address Array Pointer
    pub const OP_CONFIG: u64 = 0x38;
    
    // Runtime Registers (base + RTSOFF)
    pub const RT_IMAN: u64 = 0x20;      // Interrupter Management
    pub const RT_IMOD: u64 = 0x24;      // Interrupter Moderation
    pub const RT_ERSTSZ: u64 = 0x28;    // Event Ring Segment Table Size
    pub const RT_ERSTBA: u64 = 0x30;    // Event Ring Segment Table Base Address
    pub const RT_ERDP: u64 = 0x38;      // Event Ring Dequeue Pointer
    
    // Port Registers (base + 0x400 + port_num * 0x10)
    pub const PORT_BASE: u64 = 0x400;
    pub const PORT_PORTSC: u64 = 0x00;   // Port Status and Control
    pub const PORT_PORTPMSC: u64 = 0x04; // Port PM Status and Control
    pub const PORT_PORTLI: u64 = 0x08;   // Port Link Info
    pub const PORT_PORTHLPMC: u64 = 0x0C; // Port Hardware LPM Control
}

/// Obtiene el CAPLENGTH del controlador
pub fn get_caplength() -> Option<u8> {
    xhci_read32(offsets::CAPLENGTH).map(|val| (val & 0xFF) as u8)
}

/// Obtiene el offset de los registros operacionales
pub fn get_operational_base() -> Option<u64> {
    get_caplength().map(|cap| cap as u64)
}

/// Obtiene el offset del Runtime Registers
pub fn get_runtime_base() -> Option<u64> {
    xhci_read32(offsets::RTSOFF).map(|val| (val & !0x1F) as u64)
}

/// Obtiene el offset del Doorbell Array
pub fn get_doorbell_base() -> Option<u64> {
    xhci_read32(offsets::DBOFF).map(|val| (val & !0x3) as u64)
}

