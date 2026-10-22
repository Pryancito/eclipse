//! La ventana de registros del controlador: el reparto de la BAR en sus
//! cuatro trozos, las lecturas y escrituras de cada uno, y el handoff que le
//! quita el controlador al BIOS antes de nada.
//!
//! Sale de `xhci_hid.rs` porque es la pieza que menos tiene que ver con el
//! resto del driver: aqui no hay anillos, ni slots, ni dispositivos, solo
//! direcciones y los dos bits de un semaforo.

use super::*;

impl XhciMmio {
    pub fn from_virt(cap_base: usize, bar_size: usize) -> DeviceResult<Self> {
        if cap_base == 0 {
            return Err(DeviceError::InvalidParam);
        }
        let cap = cap_base;
        let caplength = (unsafe { read_volatile(cap as *const u32) } & 0xFF) as u64;
        let rtsoff = (unsafe { read_volatile((cap + 0x18) as *const u32) } & 0xFFFF_FFFC) as u64;
        let dboff = (unsafe { read_volatile((cap + 0x14) as *const u32) } & 0xFFFF_FFFC) as u64;
        // Cada uno de estos es la base de una ventana de registros, asi que lo
        // que tiene que caber es la base MAS un registro de 32 bits. Con
        // `> bar_size` una base podia quedarse justo en el final de la BAR --o
        // a cuatro bytes de el-- y cada acceso a traves de ella caia fuera.
        if caplength as usize + 4 > bar_size
            || rtsoff as usize + 4 > bar_size
            || dboff as usize + 4 > bar_size
        {
            return Err(DeviceError::InvalidParam);
        }
        Ok(Self {
            cap_base: cap,
            cap_len: caplength,
            op_base: cap + caplength as usize,
            rt_base: cap + rtsoff as usize,
            db_base: cap + dboff as usize,
            bar_size,
        })
    }

    pub(super) fn read_cap(&self, o: usize) -> u32 {
        fence(Ordering::Acquire);
        unsafe { read_volatile((self.cap_base + o) as *const u32) }
    }

    pub(super) fn write_cap(&self, o: usize, v: u32) {
        fence(Ordering::Release);
        unsafe {
            write_volatile((self.cap_base + o) as *mut u32, v);
        }
        fence(Ordering::Release);
        // Same seam as the operational and runtime windows. The reaction this
        // one carries is the BIOS dropping the controller when the OS asks for
        // the semaphore: without it the handoff can only be seen timing out.
        #[cfg(test)]
        xhci_fake::on_write(self.cap_base, o, v);
    }

    /// xHCI xECP: USB Legacy Support Capability — ceder control a la OS y desactivar SMI (metal).
    pub(super) fn perform_bios_handoff(&self) {
        let hcc1 = self.read_cap(0x10);
        let xecp = (hcc1 >> 16) as usize;
        if xecp == 0 {
            return;
        }
        let mut cap_ptr = xecp << 2;
        let mut cap_steps = 0usize;
        // `cap_ptr + 4`, no `cap_ptr`: el encabezado de capacidad que se lee
        // debajo son cuatro bytes, y una BAR podria no estar alineada a dword.
        while cap_ptr != 0 && cap_ptr + 4 <= self.bar_size {
            if cap_steps >= XHCI_MAX_XECP_TRAVERSAL {
                warn!("[xhci] xECP chain demasiado larga/cíclica, abortando handoff");
                break;
            }
            cap_steps = cap_steps.saturating_add(1);
            let cap_val = self.read_cap(cap_ptr);
            let cap_id = (cap_val & 0xff) as u8;
            if cap_id == 1 {
                // USB Legacy Support
                let mut legsup = self.read_cap(cap_ptr);
                if (legsup & (1 << 16)) != 0 {
                    info!("[xhci] BIOS posee el controlador, solicitando handoff...");
                    legsup |= 1 << 24; // OS Owned Semaphore
                    self.write_cap(cap_ptr, legsup);

                    let start = timer_now_us();
                    let mut spins = 0u64;
                    let max_spins = xhci_wait_spin_limit(500_000);
                    while (self.read_cap(cap_ptr) & (1 << 16)) != 0
                        && (timer_now_us() - start) < 500_000
                        && spins < max_spins
                    {
                        spins = spins.saturating_add(1);
                        spin_loop();
                    }
                    if spins >= max_spins {
                        warn!("[xhci] handoff alcanzó guard de spins (timer estancado?)");
                    }

                    if (self.read_cap(cap_ptr) & (1 << 16)) != 0 {
                        warn!("[xhci] handoff fallido por timeout, forzando control");
                    } else {
                        info!("[xhci] handoff completado con éxito");
                    }
                }

                // Desactivar SMIs y limpiar estados pendientes (USBLEGCTLSTS = offset 4)
                // Escribir 0xFFFF0000 para limpiar bits RW1C y desactivar enable bits.
                //
                // El registro son CUATRO bytes en `cap_ptr + 4`, asi que lo que
                // tiene que caber es `cap_ptr + 8`. Con `cap_ptr + 4 <= bar_size`
                // una capacidad legacy en el ultimo dword de la BAR pasaba la
                // comprobacion y el `write_volatile` se iba entero fuera de la
                // ventana: un dword de 0xffff0000 en lo que hubiera detras,
                // que en PCI es un abort o la MMIO del vecino. Medido con una
                // BAR falsa, el dword siguiente al final salia escrito.
                if cap_ptr + 8 <= self.bar_size {
                    self.write_cap(cap_ptr + 4, 0xffff_0000);
                } else {
                    warn!(
                        "[xhci] capacidad legacy en 0x{:x} sin sitio para USBLEGCTLSTS \
                         dentro de la BAR ({} bytes), SMIs sin desactivar",
                        cap_ptr, self.bar_size
                    );
                }
                return;
            }
            let next = ((cap_val >> 8) & 0xff) as usize;
            if next == 0 {
                break;
            }
            cap_ptr = cap_ptr.saturating_add(next << 2);
        }
    }

    pub(super) fn ack_host_interrupt(&self) {
        let usbsts = self.read_op(0x04);
        if (usbsts & 0x08) != 0 {
            self.write_op(0x04, 0x08);
        }
        let iman = self.read_rt(0x20);
        self.write_rt(0x20, (iman & 0x02) | 0x01);
    }

    /// The offset within the BAR of an access through one of the windows
    /// below, which is the only name the fake controller of the test binary
    /// knows a register by.
    #[cfg(test)]
    pub(super) fn bar_off(&self, addr: usize) -> usize {
        addr - self.cap_base
    }

    pub(super) fn read_op(&self, o: usize) -> u32 {
        #[cfg(test)]
        xhci_fake::on_read(self.cap_base, self.bar_off(self.op_base + o));
        fence(Ordering::Acquire);
        let v = unsafe { read_volatile((self.op_base + o) as *const u32) };
        fence(Ordering::Acquire);
        v
    }

    pub(super) fn write_op(&self, o: usize, v: u32) {
        fence(Ordering::Release);
        unsafe { write_volatile((self.op_base + o) as *mut u32, v) }
        fence(Ordering::Release);
        #[cfg(test)]
        xhci_fake::on_write(self.cap_base, self.bar_off(self.op_base + o), v);
    }

    pub(super) fn write_op64(&self, o: usize, v: u64) {
        fence(Ordering::Release);
        unsafe { write_volatile((self.op_base + o) as *mut u64, v) }
        fence(Ordering::Release);
        // The fake is told in halves, low first, because that is the order a
        // controller latches a 64-bit register pair it is allowed to read
        // between the two halves.
        #[cfg(test)]
        {
            let off = self.bar_off(self.op_base + o);
            xhci_fake::on_write(self.cap_base, off, v as u32);
            xhci_fake::on_write(self.cap_base, off + 4, (v >> 32) as u32);
        }
    }

    pub(super) fn write_rt(&self, o: usize, v: u32) {
        fence(Ordering::Release);
        unsafe { write_volatile((self.rt_base + o) as *mut u32, v) }
        fence(Ordering::Release);
        #[cfg(test)]
        xhci_fake::on_write(self.cap_base, self.bar_off(self.rt_base + o), v);
    }

    pub(super) fn write_rt64(&self, o: usize, v: u64) {
        fence(Ordering::Release);
        unsafe { write_volatile((self.rt_base + o) as *mut u64, v) }
        fence(Ordering::Release);
        #[cfg(test)]
        {
            let off = self.bar_off(self.rt_base + o);
            xhci_fake::on_write(self.cap_base, off, v as u32);
            xhci_fake::on_write(self.cap_base, off + 4, (v >> 32) as u32);
        }
    }

    pub(super) fn read_rt(&self, o: usize) -> u32 {
        #[cfg(test)]
        xhci_fake::on_read(self.cap_base, self.bar_off(self.rt_base + o));
        fence(Ordering::Acquire);
        let v = unsafe { read_volatile((self.rt_base + o) as *const u32) };
        fence(Ordering::Acquire);
        v
    }

    pub fn ring_db(&self, slot: u8, doorbell: u8) {
        fence(Ordering::Release);
        unsafe {
            write_volatile(
                (self.db_base + (slot as usize) * 4) as *mut u32,
                doorbell as u32,
            );
        }
        fence(Ordering::Release);
        #[cfg(test)]
        xhci_fake::on_write(
            self.cap_base,
            self.bar_off(self.db_base + (slot as usize) * 4),
            doorbell as u32,
        );
    }
}
