//! USB HID (Human Interface Device) - Gaming Peripherals Support
//! 
//! Rewrite using the 'xhci' crate for robust hardware compatibility.

use alloc::vec::Vec;
use alloc::format;
use core::ptr::{read_volatile, write_volatile};
use crate::memory::{phys_to_virt, virt_to_phys, alloc_dma_buffer};
use core::num::NonZeroUsize;

/// Capacidades de dispositivos gaming
#[derive(Debug, Clone, Copy)]
pub struct GamingDeviceCapabilities {
    pub vendor_id: u16,
    pub product_id: u16,
    pub max_polling_rate: u32,  // Hz
    pub max_dpi: u32,            // For mice
    pub adjustable_dpi: bool,
    pub extra_buttons: u8,
    pub n_key_rollover: bool,    // For keyboards
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
            Err("Failed to allocate DMA buffer")
        }
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

/// Generic TRB Ring management
pub struct XhciRing {
    pub dma: DmaAllocation,
    pub num_trbs: usize,
    pub enqueue_index: usize,
    pub cycle_state: bool,
}

impl XhciRing {
    pub fn new(num_trbs: usize) -> Result<Self, &'static str> {
        let dma = DmaAllocation::new(num_trbs * 16, 64)?;
        Ok(Self {
            dma,
            num_trbs,
            enqueue_index: 0,
            cycle_state: true,
        })
    }

    pub fn push_trb(&mut self, mut trb: [u32; 4]) {
        // Set cycle bit
        if self.cycle_state {
            trb[3] |= 1;
        } else {
            trb[3] &= !1;
        }

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

/// Mapper for the xhci crate to access MMIO via our HHDM
#[derive(Clone, Copy)]
pub struct XhciMapper;
impl xhci::accessor::Mapper for XhciMapper {
    unsafe fn map(&mut self, phys_base: usize, _bytes: usize) -> NonZeroUsize {
        NonZeroUsize::new(phys_to_virt(phys_base as u64) as usize).unwrap()
    }
    fn unmap(&mut self, _virt_base: usize, _bytes: usize) {}
}

pub struct XhciController {
    bus: u8,
    dev: u8,
    func: u8,
    bar0: u64,
}

impl XhciController {
    pub fn new(bus: u8, dev: u8, func: u8, bar0: u64) -> Self {
        Self { bus, dev, func, bar0 }
    }

    pub fn init(&self) -> Result<(), &'static str> {
        crate::serial::serial_print(&format!("[USB-XHCI] Initializing XHCI at {:02X}:{:02X}.{} (BAR0: 0x{:X})\n", 
            self.bus, self.dev, self.func, self.bar0));

        let mapper = XhciMapper;
        // Timeout conservador: 500_000 iteraciones (~50ms aprox en bare metal)
        const TIMEOUT: usize = 500_000;

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
                let cap_base = (self.bar0 as usize + xecp * 4) as *mut u32;
                let legsup = read_volatile(cap_base);
                crate::serial::serial_print(&format!("[USB-XHCI] LEGSUP before handoff: 0x{:08X}\n", legsup));
                // Si BIOS OS Owned Semaphore (bit 24) está a 0, pedir ownership
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
            crate::serial::serial_print("[USB-XHCI] Step 2: Controller halted\n");

            // 3. Reset del controlador
            crate::serial::serial_print("[USB-XHCI] Step 3: Resetting controller...\n");
            operational.usbcmd.update_volatile(|u| { u.set_host_controller_reset(); });
            let mut timeout = TIMEOUT;
            while operational.usbcmd.read_volatile().host_controller_reset() {
                if timeout == 0 {
                    crate::serial::serial_print("[USB-XHCI] WARN: HC Reset timeout, skipping controller\n");
                    return Ok(());
                }
                timeout -= 1;
                core::hint::spin_loop();
            }
            crate::serial::serial_print("[USB-XHCI] Step 3: Reset done\n");

            // SPEC XHCI §4.2: Tras reset, esperar de nuevo CNR antes de cualquier acceso
            crate::serial::serial_print("[USB-XHCI] Step 3b: Waiting CNR after reset...\n");
            let mut timeout = TIMEOUT;
            while operational.usbsts.read_volatile().controller_not_ready() {
                if timeout == 0 {
                    crate::serial::serial_print("[USB-XHCI] WARN: Post-reset CNR timeout, skipping controller\n");
                    return Ok(());
                }
                timeout -= 1;
                core::hint::spin_loop();
            }
            crate::serial::serial_print("[USB-XHCI] Step 3b: CNR cleared after reset\n");

            // 4. Max Slots
            let max_slots = capability.hcsparams1.read_volatile().number_of_device_slots();
            crate::serial::serial_print(&format!("[USB-XHCI] Step 4: Max slots = {}\n", max_slots));
            operational.config.update_volatile(|c| { c.set_max_device_slots_enabled(max_slots); });

            // 5. DCBAA
            crate::serial::serial_print("[USB-XHCI] Step 5: DCBAA...\n");
            let dcbaa = DmaAllocation::new((max_slots as usize + 1) * 8, 4096)?;
            operational.dcbaap.update_volatile(|d| { d.set(dcbaa.phys_addr); });

            // 6. Command Ring
            crate::serial::serial_print("[USB-XHCI] Step 6: Command Ring...\n");
            let cmd_ring = XhciRing::new(256)?;
            operational.crcr.update_volatile(|c| {
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
            
            // Port Check
            let num_ports = capability.hcsparams1.read_volatile().number_of_ports();
            crate::serial::serial_print(&format!("[USB-XHCI] Checking {} ports...\n", num_ports));
            for i in 0..num_ports {
                let port_set = regs.port_register_set.read_volatile_at(i as usize);
                let portsc = port_set.portsc;
                if portsc.current_connect_status() {
                    crate::serial::serial_print(&format!("[USB-XHCI] Port {}: Active device detected\n", i));
                }
            }
        }

        Ok(())
    }
}

pub fn init() {
    let controllers = crate::pci::find_usb_controllers();
    for dev in controllers {
        if dev.prog_if == 0x30 {
            let xhci = XhciController::new(dev.bus, dev.device, dev.function, dev.bar0 & !0xF);
            let _ = xhci.init();
        }
    }
}
