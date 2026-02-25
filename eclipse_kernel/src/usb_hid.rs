//! USB HID (Human Interface Device) - Gaming Peripherals Support
//!
//! Uses the 'xhci' crate for robust XHCI hardware access.
//! Persistent state (DMA rings, contexts) is kept in a static Mutex so
//! the hardware never points to freed memory.

use alloc::vec::Vec;
use alloc::format;
use core::ptr::{read_volatile, write_volatile};
use crate::memory::{phys_to_virt, alloc_dma_buffer};
use core::num::NonZeroUsize;
use spin::Mutex;

// ============================================================================
// Timeout constant (conservative: ~50 ms on bare metal at ~10 M iter/s)
// ============================================================================
const TIMEOUT: usize = 500_000;

// ============================================================================
// DMA allocation helper
// ============================================================================
pub struct DmaAllocation {
    pub virt_addr: u64,
    pub phys_addr: u64,
    pub size: usize,
}

impl DmaAllocation {
    pub fn new(size: usize, align: usize) -> Result<Self, &'static str> {
        if let Some((ptr, phys)) = alloc_dma_buffer(size, align) {
            unsafe { core::ptr::write_bytes(ptr, 0, size); }
            Ok(Self { virt_addr: ptr as u64, phys_addr: phys, size })
        } else {
            Err("DMA alloc failed")
        }
    }

    pub fn write_u32(&self, offset: usize, val: u32) {
        if offset + 4 > self.size { return; }
        unsafe { write_volatile((self.virt_addr + offset as u64) as *mut u32, val); }
    }

    pub fn read_u32(&self, offset: usize) -> u32 {
        if offset + 4 > self.size { return 0; }
        unsafe { read_volatile((self.virt_addr + offset as u64) as *const u32) }
    }

    pub fn write_u64(&self, offset: usize, val: u64) {
        if offset + 8 > self.size { return; }
        unsafe { write_volatile((self.virt_addr + offset as u64) as *mut u64, val); }
    }

    pub fn read_u64(&self, offset: usize) -> u64 {
        if offset + 8 > self.size { return 0; }
        unsafe { read_volatile((self.virt_addr + offset as u64) as *const u64) }
    }

    pub fn write_bytes(&self, offset: usize, data: &[u8]) {
        if offset + data.len() > self.size { return; }
        let ptr = (self.virt_addr + offset as u64) as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len()); }
    }

    pub fn read_bytes(&self, offset: usize, data: &mut [u8]) {
        if offset + data.len() > self.size { return; }
        let ptr = (self.virt_addr + offset as u64) as *const u8;
        unsafe { core::ptr::copy_nonoverlapping(ptr, data.as_mut_ptr(), data.len()); }
    }
}

// ============================================================================
// XHCI TRB ring
// ============================================================================
pub struct XhciRing {
    pub dma: DmaAllocation,
    pub num_trbs: usize,
    pub enqueue_index: usize,
    pub cycle_state: bool,
}

impl XhciRing {
    pub fn new(num_trbs: usize) -> Result<Self, &'static str> {
        let dma = DmaAllocation::new(num_trbs * 16, 64)?;
        Ok(Self { dma, num_trbs, enqueue_index: 0, cycle_state: true })
    }

    /// Write a TRB to the ring (sets cycle bit) and advance the enqueue pointer.
    pub fn push_trb(&mut self, mut trb: [u32; 4]) {
        if self.cycle_state { trb[3] |= 1; } else { trb[3] &= !1; }
        let offset = self.enqueue_index * 16;
        self.dma.write_bytes(offset, unsafe {
            core::slice::from_raw_parts(trb.as_ptr() as *const u8, 16)
        });
        self.enqueue_index += 1;
        if self.enqueue_index >= self.num_trbs - 1 {
            self.enqueue_index = 0;
            self.cycle_state = !self.cycle_state;
        }
    }
}

// ============================================================================
// xhci crate mapper (HHDM)
// ============================================================================
#[derive(Clone, Copy)]
pub struct XhciMapper;
impl xhci::accessor::Mapper for XhciMapper {
    unsafe fn map(&mut self, phys_base: usize, _bytes: usize) -> NonZeroUsize {
        // Guard: only reject zero. Sub-page-aligned addresses are valid for
        // MMIO — the xhci accessor crate maps individual register groups at
        // their exact physical offset (BAR+2, BAR+4, etc.).
        // The fault recovery point in usb_hid::init() is the real safety net
        // for addresses that have no page table mapping.
        if phys_base == 0 {
            crate::serial::serial_printf(format_args!(
                "[XHCI-MAP] WARN: phys_base=0, using dummy sentinel\n"
            ));
            static DUMMY_PAGE: [u8; 4096] = [0u8; 4096];
            return NonZeroUsize::new(DUMMY_PAGE.as_ptr() as usize).unwrap();
        }
        let virt = phys_to_virt(phys_base as u64) as usize;
        NonZeroUsize::new(virt).unwrap()
    }
    fn unmap(&mut self, _virt_base: usize, _bytes: usize) {}
}

// ============================================================================
// Persistent XHCI runtime state (keeps DMA allocations alive)
// ============================================================================
struct XhciRuntime {
    bar0: u64,
    num_ports: u8,
    _dcbaa: DmaAllocation,
    cmd_ring: XhciRing,
    event_ring: DmaAllocation,
    _erst: DmaAllocation,
    event_dequeue_phys: u64,
    event_cycle: bool,
    // Per-port HID interrupt transfer ring and report buffer (index = port number)
    hid_rings: Vec<Option<XhciRing>>,
    hid_bufs: Vec<Option<DmaAllocation>>,
}

static XHCI_RUNTIME: Mutex<Option<XhciRuntime>> = Mutex::new(None);

// ============================================================================
// USB HID Usage (page 0x07) to PS/2 Set-1 make scancode table
// ============================================================================
/// Maps USB HID keyboard usage IDs (0x04..=0x73) to PS/2 Set-1 make scancodes.
/// 0x00 means "no mapping".
#[rustfmt::skip]
const HID_TO_PS2: [u8; 0x74] = [
    // 0x00-0x03: reserved / error / POST fail / undefined
    0x00, 0x00, 0x00, 0x00,
    // 0x04 a, 0x05 b, ... 0x1D z
    0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32,
    0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    // 0x1E 1, 0x1F 2, 0x20 3, 0x21 4, 0x22 5, 0x23 6, 0x24 7, 0x25 8, 0x26 9, 0x27 0
    0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
    // 0x28 Enter, 0x29 Escape, 0x2A Backspace, 0x2B Tab
    0x1C, 0x01, 0x0E, 0x0F,
    // 0x2C Space, 0x2D -, 0x2E =, 0x2F [, 0x30 ], 0x31 \, 0x32 #, 0x33 ;, 0x34 ', 0x35 `, 0x36 ,, 0x37 ., 0x38 /
    0x39, 0x0C, 0x0D, 0x1A, 0x1B, 0x2B, 0x2B, 0x27, 0x28, 0x29, 0x33, 0x34, 0x35,
    // 0x39 CapsLock
    0x3A,
    // 0x3A F1, 0x3B F2, ... 0x45 F12
    0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42, 0x43, 0x44, 0x57, 0x58,
    // 0x46 PrintScreen, 0x47 ScrollLock, 0x48 Pause
    0x00, 0x46, 0x00,
    // 0x49 Insert, 0x4A Home, 0x4B PageUp, 0x4C Delete, 0x4D End, 0x4E PageDown
    0x00, 0x00, 0x00, 0x53, 0x00, 0x00,
    // 0x4F Right, 0x50 Left, 0x51 Down, 0x52 Up
    0x00, 0x00, 0x00, 0x00,
    // 0x53 NumLock, 0x54 KP/, 0x55 KP*, 0x56 KP-, 0x57 KP+, 0x58 KPEnter
    0x45, 0x00, 0x37, 0x4A, 0x4E, 0x00,
    // 0x59 KP1 .. 0x61 KP9
    0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49,
    // 0x62 KP0, 0x63 KP.
    0x52, 0x53,
    // 0x64 \|, 0x65 App, 0x66 Power
    0x56, 0x00, 0x00,
    // 0x67 KP=, 0x68..0x6F F13..F20
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0x70..0x73
    0x00, 0x00, 0x00, 0x00,
];

/// Translate a USB HID keyboard usage ID to a PS/2 Set-1 make scancode.
fn hid_key_to_ps2(usage: u8) -> u8 {
    if (usage as usize) < HID_TO_PS2.len() { HID_TO_PS2[usage as usize] } else { 0 }
}

// ============================================================================
// HID boot protocol report structures
// ============================================================================
/// 8-byte USB HID boot-protocol keyboard report
#[repr(C)]
struct HidKeyboardReport {
    modifier: u8,
    _reserved: u8,
    keycodes: [u8; 6],
}

/// 3-byte USB HID boot-protocol mouse report
#[repr(C)]
struct HidMouseReport {
    buttons: u8,
    x: i8,
    y: i8,
}

// ============================================================================
// Vendor/capability info (kept for ABI compatibility)
// ============================================================================
#[derive(Debug, Clone, Copy)]
pub struct GamingDeviceCapabilities {
    pub vendor_id: u16,
    pub product_id: u16,
    pub max_polling_rate: u32,
    pub max_dpi: u32,
    pub adjustable_dpi: bool,
    pub extra_buttons: u8,
    pub n_key_rollover: bool,
    pub macro_keys: u8,
    pub rgb_support: bool,
}

pub mod vendors {
    pub const LOGITECH: u16 = 0x046D;
    pub const RAZER: u16 = 0x1532;
    pub const CORSAIR: u16 = 0x1B1C;
    pub const STEELSERIES: u16 = 0x1038;
    pub const ROCCAT: u16 = 0x1E7D;
}

// ============================================================================
// XHCI controller initialisation
// ============================================================================
fn init_xhci(bus: u8, dev: u8, func: u8, bar0: u64) -> Result<(), &'static str> {
    crate::serial::serial_print(&format!(
        "[USB-XHCI] Init {:02X}:{:02X}.{} BAR0=0x{:X}\n",
        bus, dev, func, bar0
    ));

    let mapper = XhciMapper;

        unsafe {
            let mut regs = xhci::Registers::new(self.bar0 as usize, mapper);
            let operational = &mut regs.operational;
            let capability = &regs.capability;

            // --- BIOS Handoff (xHCI Extended Capabilities LEGSUP) ---
            // Leer HCCPARAMS1 para obtener xECP offset
            let hccparams1 = capability.hccparams1.read_volatile();
            let xecp = hccparams1.xhci_extended_capabilities_pointer() as usize;
            crate::serial::serial_print(&format!("[USB-XHCI] xECP offset: 0x{:X}\n", xecp));
            if xecp != 0 {
                let cap_base = phys_to_virt(self.bar0 + (xecp * 4) as u64) as *mut u32;
                let legsup = read_volatile(cap_base);
                crate::serial::serial_print(&format!("[USB-XHCI] LEGSUP before handoff: 0x{:08X}\n", legsup));
                // Si OS Owned Semaphore (bit 24) no está activo, pedir ownership
                if (legsup & (1 << 24)) == 0 {
                    write_volatile(cap_base, legsup | (1 << 24)); // Set OS Owned Semaphore
                    // Esperar a que BIOS libere (bit 16 = BIOS Owned Semaphore)
                    let mut t = 500_000usize;
                    loop {
                        let v = read_volatile(cap_base);
                        if (v & (1 << 16)) == 0 { break; } // BIOS liberó
                        if t == 0 {
                            crate::serial::serial_print("[USB-XHCI] WARN: BIOS handoff timeout, forcing ownership\n");
                            // Forzar: limpiar bit BIOS owned
                            write_volatile(cap_base, v & !(1 << 16));
                            break;
                        }
                        t -= 1;
                        core::hint::spin_loop();
                    }
                    crate::serial::serial_print(&format!("[USB-XHCI] LEGSUP after handoff: 0x{:08X}\n", read_volatile(cap_base)));
                } else {
                    crate::serial::serial_print("[USB-XHCI] OS already owns XHCI (bit 24 set)\n");
                }
            }

            // 1. Wait for CNR (Controller Not Ready)
            crate::serial::serial_print("[USB-XHCI] Step 1: Waiting for CNR...\n");
            let mut timeout = TIMEOUT;
            while operational.usbsts.read_volatile().controller_not_ready() {
                if timeout == 0 {
                    crate::serial::serial_print("[USB-XHCI] WARN: CNR timeout, skipping controller\n");
                    return Ok(()); // No bloquear el arranque
                }
                timeout -= 1;
                core::hint::spin_loop();
            }
            crate::serial::serial_print("[USB-XHCI] Step 1: CNR cleared\n");

            // 2. Stop the controller si no está parado
            crate::serial::serial_print("[USB-XHCI] Step 2: Stopping controller...\n");
            if !operational.usbsts.read_volatile().hc_halted() {
                operational.usbcmd.update_volatile(|u| { u.clear_run_stop(); });
                let mut timeout = TIMEOUT;
                while !operational.usbsts.read_volatile().hc_halted() {
                    if timeout == 0 {
                        crate::serial::serial_print("[USB-XHCI] WARN: Stop timeout, skipping controller\n");
                        return Ok(());
                    }
                    timeout -= 1;
                    core::hint::spin_loop();
                }
            }
        }

        // 1. Wait for CNR
        let mut t = TIMEOUT;
        while regs.operational.usbsts.read_volatile().controller_not_ready() {
            if t == 0 { crate::serial::serial_print("[USB-XHCI] WARN: CNR timeout\n"); return Ok(()); }
            t -= 1; core::hint::spin_loop();
        }

        // 2. Stop if running
        if !regs.operational.usbsts.read_volatile().hc_halted() {
            regs.operational.usbcmd.update_volatile(|u| { u.clear_run_stop(); });
            let mut t = TIMEOUT;
            while !regs.operational.usbsts.read_volatile().hc_halted() {
                if t == 0 { crate::serial::serial_print("[USB-XHCI] WARN: stop timeout\n"); return Ok(()); }
                t -= 1; core::hint::spin_loop();
            }
        }

        // 3. Reset
        regs.operational.usbcmd.update_volatile(|u| { u.set_host_controller_reset(); });
        let mut t = TIMEOUT;
        while regs.operational.usbcmd.read_volatile().host_controller_reset() {
            if t == 0 { crate::serial::serial_print("[USB-XHCI] WARN: reset timeout\n"); return Ok(()); }
            t -= 1; core::hint::spin_loop();
        }
        // Post-reset CNR
        let mut t = TIMEOUT;
        while regs.operational.usbsts.read_volatile().controller_not_ready() {
            if t == 0 { crate::serial::serial_print("[USB-XHCI] WARN: post-reset CNR timeout\n"); return Ok(()); }
            t -= 1; core::hint::spin_loop();
        }

        // 4. Max slots
        let max_slots = regs.capability.hcsparams1.read_volatile().number_of_device_slots();
        regs.operational.config.update_volatile(|c| { c.set_max_device_slots_enabled(max_slots); });

        // 5. DCBAA
        let dcbaa = DmaAllocation::new((max_slots as usize + 1) * 8, 4096)?;
        regs.operational.dcbaap.update_volatile(|d| { d.set(dcbaa.phys_addr); });

        // 6. Command ring
        let cmd_ring = XhciRing::new(256)?;
        regs.operational.crcr.update_volatile(|c| {
            c.set_command_ring_pointer(cmd_ring.dma.phys_addr);
            c.set_ring_cycle_state();
        });

            // 7. Event Ring
            crate::serial::serial_print("[USB-XHCI] Step 7: Event Ring...\n");
            let event_ring = DmaAllocation::new(4096, 64)?;
            let erst = DmaAllocation::new(16, 64)?;
            erst.write_bytes(0, &event_ring.phys_addr.to_le_bytes());
            erst.write_bytes(8, &(256u32).to_le_bytes());
            
            let mut interrupter0 = regs.interrupter_register_set.interrupter_mut(0);
            interrupter0.iman.update_volatile(|i| { i.set_interrupt_enable(); });
            interrupter0.erstsz.update_volatile(|e| { e.set(1); });
            interrupter0.erstba.update_volatile(|e| { e.set(erst.phys_addr); });
            interrupter0.erdp.update_volatile(|e| { e.set_event_ring_dequeue_pointer(event_ring.phys_addr); });

            // 8. Arrancar
            crate::serial::serial_print("[USB-XHCI] Step 8: Starting controller...\n");
            operational.usbcmd.update_volatile(|u| {
                u.set_interrupter_enable();
                u.set_run_stop();
            });
            
            let mut timeout = TIMEOUT;
            while operational.usbsts.read_volatile().hc_halted() {
                if timeout == 0 {
                    crate::serial::serial_print("[USB-XHCI] WARN: Controller start timeout, skipping\n");
                    return Ok(());
                }
                timeout -= 1;
                core::hint::spin_loop();
            }

            crate::serial::serial_print("[USB-XHCI] Controller fully active\n");
            
            // Port Check — con timeout por puerto para no bloquearse en hardware real
            // PORTSC bits:
            //   [0]    CCS  = Current Connect Status
            //   [9]    PP   = Port Power
            //   [8:5]  PLS  = Port Link State (0=U0, 5=RxDetect, 7=Polling)
            //   [13:10] Speed (1=FullSpeed, 2=LowSpeed, 3=HiSpeed, 4=SuperSpeed)
            //   [17:16] IPM  = Interface Power Management (2=U2=Suspend, 3=U3=SS-Disabled)
            let num_ports = capability.hcsparams1.read_volatile().number_of_ports();
            crate::serial::serial_print(&format!("[USB-XHCI] Checking {} ports...\n", num_ports));

            for i in 0..num_ports {
                crate::serial::serial_print(&format!("[USB-XHCI] Port {}: reading PORTSC...\n", i));

                // Leer PORTSC directamente vía MMIO físico + HHDM para mayor robustez.
                // Evitamos read_volatile_at() del xhci crate porque puede bloquearse
                // en hardware real si el puerto no responde inmediatamente.
                // PORTSC offset = capability_length + 0x400 + port_index * 0x10
                // (xHCI spec tabla 5-18: Port Register Set base = op_base + 0x400)
                let cap_len = capability.caplength.read_volatile().get() as u64;
                let portsc_phys = self.bar0 + cap_len + 0x400 + (i as u64 * 0x10);
                let portsc_virt = phys_to_virt(portsc_phys) as *const u32;
                let portsc_raw = read_volatile(portsc_virt);

                let ccs       = portsc_raw & (1 << 0) != 0;   // Connected
                let pp        = portsc_raw & (1 << 9) != 0;   // Power
                let pls       = (portsc_raw >> 5) & 0xF;      // Link State
                let speed     = (portsc_raw >> 10) & 0xF;     // Speed

                crate::serial::serial_print(&format!(
                    "[USB-XHCI] Port {}: PORTSC=0x{:08X} CCS={} PP={} PLS={} Speed={}\n",
                    i, portsc_raw, ccs as u8, pp as u8, pls, speed
                ));

                if ccs {
                    crate::serial::serial_print(&format!("[USB-XHCI] Port {}: Active device detected\n", i));
                }
            }
        }

        // Store runtime state (keeps all DMA allocations alive)
        *XHCI_RUNTIME.lock() = Some(XhciRuntime {
            bar0,
            num_ports,
            _dcbaa: dcbaa,
            cmd_ring,
            event_ring,
            _erst: erst,
            event_dequeue_phys,
            event_cycle: true,
            hid_rings,
            hid_bufs,
        });
    }

        crate::serial::serial_print("[USB-XHCI] init() done\n");
        Ok(())
    }
}

pub fn init() {
    let controllers = crate::pci::find_usb_controllers();
    crate::serial::serial_print(&format!("[USB-XHCI] Found {} USB controller(s)\n", controllers.len()));
    for dev in controllers {
        if dev.prog_if == 0x30 {
            let bar0_clean = dev.bar0 & !0xF;
            crate::serial::serial_print(&format!(
                "[USB-XHCI] Controller PCI {:02X}:{:02X}.{} prog_if=0x{:02X} BAR0_raw=0x{:X} BAR0_clean=0x{:X}\n",
                dev.bus, dev.device, dev.function, dev.prog_if, dev.bar0, bar0_clean
            ));

            // --- Option 3: fault recovery wrapper ---
            // Set a recovery point before touching any MMIO. If a page fault or
            // GP fires inside xhci.init(), the exception handler will jump back
            // here (returning true) and we skip this controller gracefully.
            let faulted = unsafe { crate::interrupts::set_recovery_point() };
            if faulted {
                crate::serial::serial_print(&format!(
                    "[USB-XHCI] Controller {:02X}:{:02X}.{} caused a fault — skipping\n",
                    dev.bus, dev.device, dev.function
                ));
                // Recovery point is already cleared by the exception handler.
                continue;
            }

            let xhci = XhciController::new(dev.bus, dev.device, dev.function, bar0_clean);
            let result = xhci.init();

            // Clear recovery point now that we're safely past all MMIO accesses.
            unsafe { crate::interrupts::clear_recovery_point(); }

            crate::serial::serial_print(&format!(
                "[USB-XHCI] Controller {:02X}:{:02X}.{} init result: {}\n",
                dev.bus, dev.device, dev.function,
                if result.is_ok() { "OK" } else { "ERROR" }
            ));
        }
    }
    crate::serial::serial_print("[USB-XHCI] usb_hid::init() complete\n");
}