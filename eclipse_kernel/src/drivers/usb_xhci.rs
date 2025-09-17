//! Driver genérico xHCI (esqueleto): handoff BIOS e inicio mínimo

use crate::drivers::manager::DriverResult;
use crate::drivers::pci::PciDevice;
use core::ptr::{read_volatile, write_volatile};

pub struct XhciController {
    pub pci: PciDevice,
    pub mmio_base: u64,
}

impl XhciController {
    pub fn new(pci: PciDevice) -> Self {
        Self { pci, mmio_base: 0 }
    }

    /// Localiza BARs, realiza handoff del BIOS (XUSB2PR/USBLEGSUP) cuando aplique y habilita el controlador.
    pub fn initialize(&mut self) -> DriverResult<()> {
        // Habilitar MMIO/BusMaster en PCI
        self.pci.enable_mmio_and_bus_master();

        // BAR0 generalmente MMIO para xHCI
        let bars = self.pci.read_all_bars();
        self.mmio_base = (bars[0] & 0xFFFFFFF0) as u64;

        // Handoff BIOS (USB Legacy Support) vía xHCI Extended Capabilities
        self.perform_bios_handoff();

        // Resetear el controlador y ponerlo en marcha
        self.reset_and_run_controller();

        // Intentar encender potencia en puertos (si el hardware lo requiere)
        self.enable_port_power_basic();

        // Reset básico de puertos para forzar enumeración inicial
        self.reset_ports_basic();

        // Intentar enumerar dispositivos conectados
        self.enumerate_devices_basic();

        Ok(())
    }
}

impl XhciController {
    #[inline]
    fn mmio_read8(&self, offset: usize) -> u8 {
        unsafe { read_volatile((self.mmio_base as usize as *const u8).add(offset)) }
    }

    #[inline]
    fn mmio_read32(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.mmio_base as usize as *const u32).add(offset / 4)) }
    }

    #[inline]
    fn mmio_write32(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.mmio_base as usize as *mut u32).add(offset / 4), value) }
    }

    /// Recorre las Extended Capabilities y realiza el handoff del BIOS si existe USB Legacy Support
    fn perform_bios_handoff(&self) {
        // CAPLENGTH en 0x00 (8 bits)
        let cap_length = self.mmio_read8(0x00) as usize;
        // HCCPARAMS1 en 0x10, xECP está en bits [31:16] en unidades de dwords desde base
        let hccparams1 = self.mmio_read32(0x10);
        let mut xecp_dw_offset = ((hccparams1 >> 16) & 0xFFFF) as usize; // dword offset
        if xecp_dw_offset == 0 {
            return;
        }

        // Iterar lista enlazada de extended caps
        let base = 0usize; // offsets relativos a base de capability regs
        for _ in 0..32 {
            let ext_off = base + (xecp_dw_offset * 4);
            let cap = self.mmio_read32(ext_off);
            let cap_id = (cap & 0xFF) as u8;
            let next_ptr = ((cap >> 8) & 0xFF) as u8; // en dwords

            // USB Legacy Support (ID = 1)
            if cap_id == 0x01 {
                let usblegsup = self.mmio_read32(ext_off);
                // Bits de ownership: BIOS (bit16), OS (bit24)
                let mut new_usblegsup = usblegsup | (1 << 24);
                self.mmio_write32(ext_off, new_usblegsup);

                // Esperar a que BIOS Owned (bit16) se limpie
                let mut tries = 0;
                while tries < 100000 {
                    let v = self.mmio_read32(ext_off);
                    if (v & (1 << 16)) == 0 {
                        break;
                    }
                    tries += 1;
                }

                // Deshabilitar SMI en USBLEGCTLSTS si existe (siguiente dword)
                let legctl_off = ext_off + 4;
                let mut legctl = self.mmio_read32(legctl_off);
                // Común: limpiar bits de SMI enable si presentes
                legctl &= !0xFFFF_FFFF; // conservador: deshabilitar todo
                self.mmio_write32(legctl_off, legctl);

                break;
            }

            if next_ptr == 0 { break; }
            xecp_dw_offset = next_ptr as usize;
        }

        // Evitar warnings por variables no usadas
        let _ = cap_length;
    }

    /// Reset básico del controlador y poner RS=1
    fn reset_and_run_controller(&self) {
        let cap_length = self.mmio_read8(0x00) as usize;
        let op_base = cap_length; // Operational regs base
        let usbcmd_off = op_base + 0x00;
        let usbsts_off = op_base + 0x04;

        // Solicitar reset (HCRST bit1)
        let mut usbcmd = self.mmio_read32(usbcmd_off);
        usbcmd |= 1 << 1;
        self.mmio_write32(usbcmd_off, usbcmd);

        // Esperar a que el reset complete (HCRST limpia y CNR en USBSTS se limpie)
        let mut tries = 0;
        while tries < 100000 {
            let st = self.mmio_read32(usbsts_off);
            // CNR es bit 11; esperamos que eventualmente se limpie tras reset
            if (st & (1 << 11)) == 0 {
                break;
            }
            tries += 1;
        }

        // Poner RS=1 para arrancar
        let mut cmd = self.mmio_read32(usbcmd_off);
        cmd |= 1 << 0;
        self.mmio_write32(usbcmd_off, cmd);
    }

    /// Intenta encender la potencia de los puertos. En muchos xHCI esto es automático,
    /// pero algunos controladores requieren establecer Port Power en PORTSC.
    fn enable_port_power_basic(&self) {
        let cap_length = self.mmio_read8(0x00) as usize;
        let op_base = cap_length;
        // HCSParams1 para número de puertos (bits 31:24) en capability regs +0x04
        let hcsparams1 = self.mmio_read32(0x04);
        let num_ports = ((hcsparams1 >> 24) & 0xFF) as usize;
        
        // En xHCI, el conjunto de puertos típicamente comienza en op_base + 0x400
        let ports_base = op_base + 0x400;
        for i in 0..num_ports {
            let portsc_off = ports_base + i * 0x10;
            let mut v = self.mmio_read32(portsc_off);
            // Bit Port Power puede ser RW; establecemos si parece estar disponible
            // Usamos máscara conservadora: set bit 9 si está a cero y no está RO
            v |= 1 << 9; // PP
            self.mmio_write32(portsc_off, v);
        }
    }

    /// Realiza un reset básico de todos los puertos disponibles (PORTSC.PR) y limpia flags de cambio
    fn reset_ports_basic(&self) {
        let cap_length = self.mmio_read8(0x00) as usize;
        let op_base = cap_length;
        let hcsparams1 = self.mmio_read32(0x04);
        let num_ports = ((hcsparams1 >> 24) & 0xFF) as usize;
        
        let ports_base = op_base + 0x400;
        for i in 0..num_ports {
            let portsc_off = ports_base + i * 0x10;
            let mut v = self.mmio_read32(portsc_off);

            // Limpiar bits de cambio antes (CSC=bit17, PEC=bit18, WRC=19, OCC=20, PRC=21, PLC=22, CEC=23)
            v |= (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21) | (1 << 22) | (1 << 23);
            self.mmio_write32(portsc_off, v);

            // Iniciar reset de puerto (PR bit4)
            let mut v2 = self.mmio_read32(portsc_off);
            v2 |= 1 << 4;
            self.mmio_write32(portsc_off, v2);

            // Esperar a que PR se limpie por hardware
            let mut tries = 0;
            while tries < 100000 {
                let cur = self.mmio_read32(portsc_off);
                if (cur & (1 << 4)) == 0 { break; }
                tries += 1;
            }

            // Limpiar cambios nuevamente
            let mut v3 = self.mmio_read32(portsc_off);
            v3 |= (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21) | (1 << 22) | (1 << 23);
            self.mmio_write32(portsc_off, v3);
        }
    }

    /// Enumeración básica de dispositivos conectados (solo detecta HID)
    fn enumerate_devices_basic(&self) {
        let cap_length = self.mmio_read8(0x00) as usize;
        let op_base = cap_length;
        let hcsparams1 = self.mmio_read32(0x04);
        let num_ports = ((hcsparams1 >> 24) & 0xFF) as usize;
        let ports_base = op_base + 0x400;
        
        for i in 0..num_ports {
            let portsc_off = ports_base + i * 0x10;
            let status = self.mmio_read32(portsc_off);
            let ccs = (status >> 0) & 1; // Current Connect Status
            let ped = (status >> 2) & 1; // Port Enabled/Disabled
            
            if ccs == 1 && ped == 1 {
                // Intentar habilitar slot y configurar dispositivo
                let _ = self.enable_slot_and_configure(i);
            }
        }
    }

    /// Habilita slot y configura dispositivo básico
    fn enable_slot_and_configure(&self, _port_num: usize) -> DriverResult<()> {
        // Por ahora solo reportamos que detectamos algo
        // TODO: Implementar Enable Slot, Address Device, y Transfer Ring setup
        // para HID (teclado/ratón) básico
        
        Ok(())
    }
}


