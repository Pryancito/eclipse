//! xHCI + USB HID: enumeración en puertos raíz, HID boot (teclado / ratón / tablet QEMU),
//! MSI + `poll()` por timer, handoff USB legacy, anillos TRB alineados a la especificación.
//!
//! **Alcance:** controladores xHCI, puertos raíz y hubs USB hasta los cinco niveles
//! que cabe nombrar en un route string (registro global de sondeo).
//! Los cambios de puerto de un hub llegan por su endpoint de cambio de estado, con
//! un barrido por transferencia de control como red de seguridad.
//! **No cubierto:** Multi-TT, descriptores HID no boot, varios interfaces HID
//! compuestos, USB3 recovery avanzado.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{_mm_clflush, _mm_mfence};
use core::hint::spin_loop;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicUsize, Ordering};

use lock::Mutex;
use pci::PCIDevice;

use crate::builder::IoMapper;
use crate::bus::drivers_timer_now_as_micros;
use crate::bus::pci_drivers::PciDriver;
use crate::bus::{phys_to_virt, PAGE_SIZE};
use crate::input::input_event_codes::{abs::*, ev::*, input_prop::*, key::*, rel::*, syn::*};
use crate::prelude::{AbsInfo, CapabilityType, InputCapability, InputEvent, InputEventType};
use crate::scheme::{impl_event_scheme, BlockScheme, InputScheme, IrqScheme, Scheme};
use crate::utils::EventListener;
use crate::{Device, DeviceError, DeviceResult};
use pci::BAR;

fn timer_now_us() -> u64 {
    unsafe { drivers_timer_now_as_micros() }
}

#[inline(always)]
fn xhci_wait_spin_limit(timeout_us: u64) -> u64 {
    // Cap spin iterations so a stuck TSC timer cannot block boot for minutes.
    // (Previously timeout_us * 50_000 allowed billions of iterations at 80%.)
    500_000_000u64.max(timeout_us.saturating_mul(500))
}

#[inline(always)]
fn xhci_wait_expired(start: u64, timeout_us: u64, spins: u64) -> bool {
    timer_now_us().wrapping_sub(start) >= timeout_us || spins >= xhci_wait_spin_limit(timeout_us)
}

#[inline(always)]
fn xhci_spin_delay_us(delay_us: u64) {
    let start = timer_now_us();
    let mut spins = 0u64;
    while !xhci_wait_expired(start, delay_us, spins) {
        spins = spins.saturating_add(1);
        spin_loop();
    }
}

// PORTSC bits RW1C (port change). Hay que mantenerlos a 0 salvo cuando queramos limpiarlos.
//
// Los siete que define xHCI 1.2 (tabla 5-27), todos RW1CS:
//   CSC (17) Connect Status Change      PEC (18) Port Enabled/Disabled Change
//   WRC (19) Warm Port Reset Change     OCC (20) Over-current Change
//   PRC (21) Port Reset Change          PLC (22) Port Link State Change
//   CEC (23) Port Config Error Change
//
// CEC faltaba, y faltar aqui es peor que no mirarlo: como tampoco entraba en
// `PORTSC_RW1C_AND_RO_MASK`, cada reescritura de una muestra de PORTSC --
// encender el puerto, lanzar un reset, reconocer un CSC -- le escribia un 1 y
// lo borraba sin que nadie lo hubiera leido. Un puerto USB3 que no consigue
// configurar su enlace levanta CEC y solo CEC: el error se perdia ahi, ningun
// camino lo contaba como cambio, y el puerto se quedaba mudo sin una linea en
// el log. QEMU no levanta CEC nunca; el hardware de verdad si.
const PORTSC_CHANGE_BITS: u32 =
    (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21) | (1 << 22) | (1 << 23);

// PORTSC bits que son RW1C pero NO son "change" flags — escribir 1 los borra.
// PED (bit 1) es RW1C: escribir 1 deshabilita el puerto. Siempre hay que enmascararlo
// cuando modificamos PORTSC para no tirar accidentalmente la habilitación del puerto.
// Bits que NUNCA hay que reescribir desde una muestra de PORTSC:
//   PED  (1)  RW1C: escribir 1 deshabilita el puerto.
//   PR   (4)  RW1S: escribir 1 reasserta un reset de puerto.
//   WPR  (31) RW1S: igual, con warm reset (USB3).
// Reescribir un `sc` muestreado segundos antes podia, por tanto, relanzar un
// reset de puerto en mitad de la vida del dispositivo.
const PORTSC_RW1C_AND_RO_MASK: u32 = PORTSC_CHANGE_BITS | (1 << 1) | (1 << 4) | (1u32 << 31); // PED, PR, WPR

/// The value to write into PORTSC, built from a sample `sc` of it.
///
/// `set` is what we actually want this write to do -- acknowledge a change bit,
/// power the port (PP), assert a reset (PR). Everything else is carried over
/// from the sample with [`PORTSC_RW1C_AND_RO_MASK`] stripped, because carrying
/// one of those bits over *does something*: PED disables the port, PR and WPR
/// relaunch a port reset in the middle of a device's life, and a change bit is
/// acknowledged without anyone having acted on it.
///
/// This expression was written out eight times, and a bit missing from the mask
/// is therefore a bug in all eight at once -- which is exactly how CEC came to
/// be cleared behind our back. One function, one place to get it right, and one
/// place for a test to reach.
#[inline]
fn portsc_writeback(sc: u32, set: u32) -> u32 {
    (sc & !PORTSC_RW1C_AND_RO_MASK) | set
}

/// Reintentos consecutivos de enumeracion por puerto antes de rendirse hasta
/// el siguiente CSC.
const PORT_ENUM_MAX_RETRIES: u8 = 3;
const XHCI_MAX_XECP_TRAVERSAL: usize = 256;
const XHCI_WAIT_SPIN_FACTOR: u64 = 50_000;

// ——— USB legacy (EHCI/OHCI/UHCI) ———
//
// El usuario pidió unificar: mantenemos el cableado aquí (aunque el nombre del
// fichero sea `xhci_hid.rs`) para evitar proliferación de módulos.

#[cfg(feature = "legacy-usb-hid")]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LegacyUsbKind {
    Uhci,
    Ohci,
    Ehci,
}

#[cfg(feature = "legacy-usb-hid")]
pub struct LegacyUsbHid {
    #[allow(dead_code)]
    listener: EventListener<InputEvent>,
    #[allow(dead_code)]
    kind: LegacyUsbKind,
}

#[cfg(feature = "legacy-usb-hid")]
impl LegacyUsbHid {
    pub fn probe(
        kind: LegacyUsbKind,
        _dev: &PCIDevice,
        _mmio_vaddr: usize,
        _bar_size: usize,
        _msi_vector: usize,
    ) -> DeviceResult<Arc<Self>> {
        let _ = kind;
        Err(DeviceError::NotSupported)
    }
}

#[cfg(feature = "legacy-usb-hid")]
impl_event_scheme!(LegacyUsbHid, InputEvent);

#[cfg(feature = "legacy-usb-hid")]
impl Scheme for LegacyUsbHid {
    fn name(&self) -> &str {
        match self.kind {
            LegacyUsbKind::Uhci => "uhci-usb-hid",
            LegacyUsbKind::Ohci => "ohci-usb-hid",
            LegacyUsbKind::Ehci => "ehci-usb-hid",
        }
    }
}

#[cfg(feature = "legacy-usb-hid")]
impl InputScheme for LegacyUsbHid {
    fn capability(&self, _cap_type: CapabilityType) -> InputCapability {
        InputCapability::empty()
    }
}

// ——— MSI diferido ———

static MSI_IRQ_HOST: Mutex<Option<Arc<dyn IrqScheme>>> = Mutex::new(None);
static MSI_PENDING: Mutex<Vec<(usize, Arc<dyn Scheme>)>> = Mutex::new(Vec::new());
const MAX_MSI_PENDING: usize = 256;
/// Which of the two MSI queues this is, for the log.
const MSI_QUEUE_OWNER: &str = "usb";

fn enqueue_pending_msi(
    pending: &mut Vec<(usize, Arc<dyn Scheme>)>,
    vector: usize,
    dev: &Arc<dyn Scheme>,
) {
    if pending
        .iter()
        .any(|(v, d)| *v == vector && core::ptr::eq(Arc::as_ptr(d), Arc::as_ptr(dev)))
    {
        return;
    }
    if pending.len() >= MAX_MSI_PENDING {
        // A dropped entry is a device that will never be told about its own
        // interrupt, so it does not go quietly. One note per device today
        // makes this unreachable, and that is exactly why it would be
        // unreachable to debug as well if it ever were reached.
        let (v, d) = pending.remove(0);
        crate::klog_warn!(
            "[{}] MSI queue full at {}: vector {} for {} dropped, that device will get no interrupt",
            MSI_QUEUE_OWNER,
            MAX_MSI_PENDING,
            v,
            d.name()
        );
    }
    pending.push((vector, dev.clone()));
}

pub fn pci_set_irq_host(irq: Arc<dyn IrqScheme>) {
    *MSI_IRQ_HOST.lock() = Some(irq);
}

pub fn pci_note_pending_msi(vector: usize, dev: Arc<dyn Scheme>) {
    enqueue_pending_msi(&mut MSI_PENDING.lock(), vector, &dev);
}

pub fn pci_finish_msi_registrations() -> DeviceResult<()> {
    let host = MSI_IRQ_HOST.lock().clone().ok_or(DeviceError::NotReady)?;
    let mut q = MSI_PENDING.lock();
    // One vector at a time, and a failure does not end the walk. With `?` here
    // the first controller that refused a vector returned from the function
    // mid-`drain`, and `Drain::drop` then threw away every entry behind it: a
    // device queued after a failing one never got its interrupt registered at
    // all. The caller is `let _ = pci_finish_msi_registrations()`, so the error
    // went nowhere either and nothing said a word. The twin of this function in
    // `net` has always logged and carried on; this is the same.
    let mut failed = 0usize;
    for (v, d) in q.drain(..) {
        // The name is copied out before the device moves into `register_device`.
        let name = alloc::string::String::from(d.name());
        match host.register_device(v, d) {
            Ok(()) => match host.unmask(v) {
                Ok(()) => crate::klog_info!("[usb] IRQ vector {} registered for {}", v, name),
                Err(e) => {
                    failed += 1;
                    crate::klog_warn!("[usb] IRQ vector {} for {} not unmasked: {:?}", v, name, e);
                }
            },
            Err(e) => {
                failed += 1;
                crate::klog_warn!(
                    "[usb] IRQ vector {} for {} could not be registered: {:?}",
                    v,
                    name,
                    e
                );
            }
        }
    }
    if failed > 0 {
        crate::klog_warn!("[usb] {} MSI vector(s) left without an interrupt", failed);
    }
    Ok(())
}

unsafe fn dma_alloc_pages(pages: usize) -> DeviceResult<(usize, usize)> {
    let p = crate::bus::drivers_dma_alloc(pages);
    if p == 0 {
        return Err(DeviceError::DmaError);
    }
    Ok((phys_to_virt(p), p))
}

struct DmaBuf {
    virt: usize,
    phys: usize,
    len: usize,
}

impl DmaBuf {
    fn new(len: usize, align: usize) -> DeviceResult<Self> {
        let pages = len.div_ceil(PAGE_SIZE);
        let ap = align.div_ceil(PAGE_SIZE);
        let pages = pages.max(ap);
        let (virt, phys) = unsafe { dma_alloc_pages(pages)? };
        unsafe {
            core::ptr::write_bytes(virt as *mut u8, 0, pages * PAGE_SIZE);
        }
        Ok(Self {
            virt,
            phys,
            len: pages * PAGE_SIZE,
        })
    }

    fn write_u32(&self, off: usize, v: u32) {
        unsafe {
            ((self.virt + off) as *mut u32).write_volatile(v);
        }
    }

    fn read_u32(&self, off: usize) -> u32 {
        unsafe { ((self.virt + off) as *const u32).read_volatile() }
    }

    fn read_u64(&self, off: usize) -> u64 {
        let lo = self.read_u32(off) as u64;
        let hi = self.read_u32(off + 4) as u64;
        lo | (hi << 32)
    }

    fn write_u64(&self, off: usize, v: u64) {
        self.write_u32(off, v as u32);
        self.write_u32(off + 4, (v >> 32) as u32);
    }

    fn read_into(&self, off: usize, dst: &mut [u8]) {
        let n = dst.len().min(self.len.saturating_sub(off));
        if n == 0 {
            return;
        }
        unsafe {
            core::ptr::copy_nonoverlapping((self.virt + off) as *const u8, dst.as_mut_ptr(), n);
        }
    }

    /// Pareja de `read_into`: mete `src` en el bufer y devuelve cuantos bytes
    /// entraron de verdad.
    ///
    /// Devuelve la cuenta en vez de `()` a proposito. Si el bufer se queda
    /// corto, una escritura que no se enterase mandaria al disco un CBW que
    /// dice «un sector» con medio sector de datos viejos detras, y el disco lo
    /// escribiria sin protestar. Quien llama compara y aborta.
    #[must_use]
    fn write_from(&self, off: usize, src: &[u8]) -> usize {
        let n = src.len().min(self.len.saturating_sub(off));
        if n == 0 {
            return 0;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), (self.virt + off) as *mut u8, n);
        }
        n
    }

    fn sub_phys(&self, off: usize) -> u64 {
        (self.phys + off) as u64
    }

    /// Give up ownership without freeing.
    ///
    /// Used on every error path where a command or a control transfer did not
    /// complete: the controller may still be mid-DMA into this buffer, and
    /// handing those pages back to the kernel allocator would let it scribble
    /// over whatever is allocated there next. Leaking a few pages beats a
    /// corruption that surfaces somewhere else entirely.
    fn leak(self) {
        core::mem::forget(self);
    }

    fn flush(&self, off: usize, len: usize) {
        #[cfg(target_arch = "x86_64")]
        {
            let mut addr = self.virt + off;
            let end = addr + len;
            while addr < end {
                unsafe {
                    _mm_clflush(addr as *const u8);
                }
                addr += 64;
            }
            unsafe {
                _mm_mfence();
            }
        }
        let _ = (off, len);
    }
}

impl Drop for DmaBuf {
    /// Return the pages to the kernel.
    ///
    /// Until this existed the driver never freed a single DMA page: every
    /// enumeration attempt leaked its input contexts and descriptor buffers
    /// (a dozen pages a go, and a failed port is retried), and unplugging a
    /// device leaked its whole device context, its transfer rings and its
    /// report buffers. Plugging a mouse in and out was a slow memory leak with
    /// no ceiling.
    ///
    /// Everything the controller can still be looking at is handed away with
    /// [`DmaBuf::leak`] instead, so reaching here means the hardware is done
    /// with these pages.
    fn drop(&mut self) {
        if self.phys == 0 || self.len == 0 {
            return;
        }
        unsafe {
            crate::bus::dma_dealloc(self.phys, self.len / PAGE_SIZE);
        }
    }
}

pub struct XhciMmio {
    cap_base: usize,
    cap_len: u64,
    pub(crate) op_base: usize,
    pub(crate) rt_base: usize,
    db_base: usize,
    bar_size: usize,
}

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

    fn read_cap(&self, o: usize) -> u32 {
        fence(Ordering::Acquire);
        unsafe { read_volatile((self.cap_base + o) as *const u32) }
    }

    fn write_cap(&self, o: usize, v: u32) {
        fence(Ordering::Release);
        unsafe {
            write_volatile((self.cap_base + o) as *mut u32, v);
        }
        fence(Ordering::Release);
    }

    /// xHCI xECP: USB Legacy Support Capability — ceder control a la OS y desactivar SMI (metal).
    fn perform_bios_handoff(&self) {
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

    fn ack_host_interrupt(&self) {
        let usbsts = self.read_op(0x04);
        if (usbsts & 0x08) != 0 {
            self.write_op(0x04, 0x08);
        }
        let iman = self.read_rt(0x20);
        self.write_rt(0x20, (iman & 0x02) | 0x01);
    }

    fn read_op(&self, o: usize) -> u32 {
        fence(Ordering::Acquire);
        let v = unsafe { read_volatile((self.op_base + o) as *const u32) };
        fence(Ordering::Acquire);
        v
    }

    fn write_op(&self, o: usize, v: u32) {
        fence(Ordering::Release);
        unsafe { write_volatile((self.op_base + o) as *mut u32, v) }
        fence(Ordering::Release);
    }

    fn write_op64(&self, o: usize, v: u64) {
        fence(Ordering::Release);
        unsafe { write_volatile((self.op_base + o) as *mut u64, v) }
        fence(Ordering::Release);
    }

    fn write_rt(&self, o: usize, v: u32) {
        fence(Ordering::Release);
        unsafe { write_volatile((self.rt_base + o) as *mut u32, v) }
        fence(Ordering::Release);
    }

    fn write_rt64(&self, o: usize, v: u64) {
        fence(Ordering::Release);
        unsafe { write_volatile((self.rt_base + o) as *mut u64, v) }
        fence(Ordering::Release);
    }

    fn read_rt(&self, o: usize) -> u32 {
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
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Trb {
    p: u64,
    status: u32,
    ctrl: u32,
}

const TRB_LINK: u32 = 6 << 10;
const TRB_EVT_CMD_COMP: u32 = 33 << 10;
const TRB_EVT_TRANSFER: u32 = 32 << 10;
const TRB_EVT_PORT_STATUS: u32 = 34 << 10;
const TRB_CC_SUCCESS: u32 = 1;
const TRB_CC_SHORT: u32 = 13;

/// The Interval field of an interrupt Endpoint Context, from the endpoint
/// descriptor's `bInterval` and the device's xHCI speed id.
///
/// xHCI always expresses Interval as an exponent M of 125 µs, but USB does not
/// say `bInterval` the same way at every speed, and the legal range of M
/// differs too (xHCI 1.2 table 6-12). Linux does this in
/// `xhci_get_endpoint_interval()`:
///
/// * High/Super speed: `bInterval` is already an exponent N of 125 µs with the
///   period `2^(N-1)`, so M = N - 1, clamped to 0..=15.
/// * Full/Low speed: `bInterval` is a count of 1 ms FRAMES, and one frame is
///   eight 125 µs microframes, so M = log2(bInterval × 8), clamped to 3..=10.
///   Passing the frame count through unchanged -- what this used to do -- asks
///   a 10 ms mouse for one report every 2^10 × 125 µs = 128 ms, twelve times
///   too slow, and a 1 ms or 2 ms endpoint lands on M = 1 or 2, outside the
///   legal range, which a strict controller answers with Parameter Error.
fn xhci_endpoint_interval(speed: u8, b_interval: u8) -> u32 {
    if speed >= 3 {
        (b_interval.clamp(1, 16) - 1) as u32
    } else {
        let microframes = (b_interval.max(1) as u32) * 8;
        // floor(log2(microframes)), i.e. Linux's fls() - 1.
        (31 - microframes.leading_zeros()).clamp(3, 10)
    }
}

/// Does this transfer-event completion code leave the endpoint in the Halted
/// state, so that it needs Reset Endpoint + Set TR Dequeue Pointer before it
/// will run again?
///
/// xHCI 1.2 table 6-90. The codes below are the ones that reach a transfer
/// event WITHOUT halting the endpoint, so they must not trigger a reset:
/// Ring Underrun/Overrun (isoch), Event Ring Full, Missed Service Error (the
/// host skipped a service interval -- routine on a busy bus), and the four
/// Stopped codes, which are the answer to a Stop Endpoint command. Anything
/// else -- Stall, Babble, Transaction Error, Data Buffer Error, TRB Error, or
/// a code we do not know -- is treated as halting, because the recovery is
/// harmless on an endpoint that did not need it and mandatory on one that did.
fn cc_halts_endpoint(cc: u32) -> bool {
    !matches!(cc, 14 | 15 | 21 | 23 | 24 | 25 | 26 | 27 | 28)
}

fn trb_link(phys: u64, cycle: bool) -> Trb {
    let mut c = TRB_LINK | (1 << 1);
    if cycle {
        c |= 1;
    }
    Trb {
        p: phys,
        status: 0,
        ctrl: c,
    }
}

fn trb_enable_slot() -> Trb {
    Trb {
        p: 0,
        status: 0,
        ctrl: (9 << 10) | 1,
    }
}

fn trb_disable_slot(slot: u8) -> Trb {
    Trb {
        p: 0,
        status: 0,
        ctrl: (10 << 10) | ((slot as u32) << 24) | 1,
    }
}

fn trb_address_device(input_ctx: u64, slot: u8) -> Trb {
    Trb {
        p: input_ctx,
        status: 0,
        ctrl: (11u32 << 10) | ((slot as u32) << 24),
    }
}

fn trb_configure_endpoint(input_ctx: u64, slot: u8) -> Trb {
    Trb {
        p: input_ctx,
        status: 0,
        ctrl: (12u32 << 10) | ((slot as u32) << 24),
    }
}

/// Evaluate Context (xHCI 1.2 section 4.6.7). This is the command that changes
/// fields of an ALREADY configured context -- EP0's Max Packet Size among
/// them. Configure Endpoint is for adding and dropping endpoints, and on a
/// controller that enforces the distinction it answers Context State Error
/// here, leaving EP0 at the guessed packet size.
fn trb_evaluate_context(input_ctx: u64, slot: u8) -> Trb {
    Trb {
        p: input_ctx,
        status: 0,
        ctrl: (13u32 << 10) | ((slot as u32) << 24),
    }
}

/// Stop Endpoint (xHCI 1.2 section 4.6.9). Tells the controller to stop
/// processing this endpoint's transfer ring, so the ring's pages can be
/// released without the hardware still walking them.
fn trb_stop_endpoint(slot: u8, dci: u8) -> Trb {
    Trb {
        p: 0,
        status: 0,
        ctrl: (15u32 << 10) | ((dci as u32) << 16) | ((slot as u32) << 24),
    }
}

fn trb_reset_endpoint(slot: u8, dci: u8) -> Trb {
    Trb {
        p: 0,
        status: 0,
        ctrl: (14u32 << 10) | ((dci as u32) << 16) | ((slot as u32) << 24),
    }
}

fn trb_set_tr_dequeue_pointer(new_deq_phys: u64, dcs: bool, slot: u8, dci: u8) -> Trb {
    Trb {
        p: (new_deq_phys & !0xf) | (dcs as u64),
        status: 0,
        ctrl: (16u32 << 10) | ((dci as u32) << 16) | ((slot as u32) << 24),
    }
}

fn trb_setup(bmrt: u8, breq: u8, wvalue: u16, windex: u16, wlen: u16, trt: u8) -> Trb {
    let param = (bmrt as u64)
        | ((breq as u64) << 8)
        | ((wvalue as u64) << 16)
        | ((windex as u64) << 32)
        | ((wlen as u64) << 48);
    Trb {
        p: param,
        status: 8,
        ctrl: (2u32 << 10) | (1u32 << 6) | ((trt as u32) << 16),
    }
}

fn trb_data(buf: u64, len: u32, is_in: bool) -> Trb {
    let mut c = 3u32 << 10;
    if is_in {
        c |= 1 << 16;
    }
    Trb {
        p: buf,
        status: len & 0x1_ffff,
        ctrl: c,
    }
}

fn trb_status(is_in: bool, ioc: bool) -> Trb {
    let mut c = 4u32 << 10;
    if is_in {
        c |= 1 << 16;
    }
    if ioc {
        c |= 1 << 5;
    }
    Trb {
        p: 0,
        status: 0,
        ctrl: c,
    }
}

fn trb_normal(buf: u64, len: u16, ioc: bool) -> Trb {
    let mut c = 1u32 << 10;
    if ioc {
        c |= 1 << 5;
    }
    Trb {
        p: buf,
        status: (len as u32) & 0x1_ffff,
        ctrl: c,
    }
}

struct CmdRing {
    buf: DmaBuf,
    cap: usize,
    enq: usize,
    cycle: bool,
}

impl CmdRing {
    /// Offset del dword de control del TRB LINK (último TRB del segmento; índice = `cap` con `cap = n - 1`).
    #[inline]
    fn link_ctrl_off(&self) -> usize {
        self.cap * 16 + 12
    }

    fn sync_link_cycle_bit(&self) {
        let link_off = self.link_ctrl_off();
        // Preservar todos los bits excepto el bit 0 (Cycle).
        // El bit 1 (Toggle Cycle) debe permanecer en 1 para que el hardware invierta su ciclo.
        let mut c = self.buf.read_u32(link_off) & !1u32;
        if self.cycle {
            c |= 1;
        }
        fence(Ordering::Release);
        self.buf.write_u32(link_off, c);
        // Asegurar que el controlador vea el cambio del bit de ciclo en el TRB LINK.
        self.buf.flush(link_off, 4);
    }

    fn new(n: usize) -> DeviceResult<Self> {
        let buf = DmaBuf::new(n * 16, 64)?;
        let link = trb_link(buf.phys as u64, true);
        let off = (n - 1) * 16;
        buf.write_u64(off, link.p);
        buf.write_u32(off + 8, link.status);
        fence(Ordering::Release);
        buf.write_u32(off + 12, link.ctrl);
        buf.flush(off, 16);
        Ok(Self {
            buf,
            cap: n - 1,
            enq: 0,
            cycle: true,
        })
    }

    fn push(&mut self, mut t: Trb) -> DeviceResult<u64> {
        let phys = (self.buf.phys + self.enq * 16) as u64;
        let cycle_bit = if self.cycle { 1u32 } else { 0 };
        t.ctrl = (t.ctrl & !1) | cycle_bit;
        let off = self.enq * 16;
        self.buf.write_u64(off, t.p);
        self.buf.write_u32(off + 8, t.status);
        fence(Ordering::Release);
        self.buf.write_u32(off + 12, t.ctrl);
        self.buf.flush(off, 16);
        self.enq += 1;
        if self.enq >= self.cap {
            // The Link TRB's Cycle bit is published with the producer cycle
            // state of the pass that just ended, and ONLY here. There used to
            // be a second `sync_link_cycle_bit()` halfway through each pass:
            // by then `self.cycle` had already been toggled, so it stamped the
            // NEXT pass's cycle onto the Link TRB while a consumer still
            // working through the tail of the current pass had not reached it
            // yet. That consumer then found a Link TRB whose Cycle bit did not
            // match its own state, stopped there, and the ring was dead for
            // good -- no more transfers, no more events, nothing to recover
            // from. (xHCI 1.2 section 4.9.2.2.)
            self.enq = 0;
            self.sync_link_cycle_bit();
            self.cycle = !self.cycle;
        }
        Ok(phys)
    }

    fn crcr(&self) -> u64 {
        self.buf.phys as u64
    }
}

/// Anillo de transferencia (EP0 / interrupción) con seguimiento de dequeue software.
struct XferRing {
    buf: DmaBuf,
    cap: usize,
    enq: usize,
    xfer_deq: usize,
    cycle: bool,
}

impl XferRing {
    #[inline]
    fn link_ctrl_off(&self) -> usize {
        self.cap * 16 + 12
    }

    fn sync_link_cycle_bit(&self) {
        let link_off = self.link_ctrl_off();
        // Preservar todos los bits excepto el bit 0 (Cycle).
        let mut c = self.buf.read_u32(link_off) & !1u32;
        if self.cycle {
            c |= 1;
        }
        fence(Ordering::Release);
        self.buf.write_u32(link_off, c);
        // Asegurar que el controlador vea el cambio del bit de ciclo en el TRB LINK.
        self.buf.flush(link_off, 4);
    }

    fn new(n: usize) -> DeviceResult<Self> {
        let buf = DmaBuf::new(n * 16, 64)?;
        let link = trb_link(buf.phys as u64, true);
        let off = (n - 1) * 16;
        buf.write_u64(off, link.p);
        buf.write_u32(off + 8, link.status);
        fence(Ordering::Release);
        buf.write_u32(off + 12, link.ctrl);
        buf.flush(off, 16);
        Ok(Self {
            buf,
            cap: n - 1,
            enq: 0,
            xfer_deq: 0,
            cycle: true,
        })
    }

    fn ring_phys(&self) -> u64 {
        self.buf.phys as u64 | 1
    }

    fn is_full(&self) -> bool {
        (self.enq + 1) % self.cap == self.xfer_deq
    }

    fn push(&mut self, mut t: Trb) -> DeviceResult<u64> {
        if self.is_full() {
            return Err(DeviceError::NoResources);
        }
        let phys = (self.buf.phys + self.enq * 16) as u64;
        let cycle_bit = if self.cycle { 1u32 } else { 0 };
        t.ctrl = (t.ctrl & !1) | cycle_bit;
        let off = self.enq * 16;
        self.buf.write_u64(off, t.p);
        self.buf.write_u32(off + 8, t.status);
        fence(Ordering::Release);
        self.buf.write_u32(off + 12, t.ctrl);
        self.buf.flush(off, 16);
        self.enq += 1;
        if self.enq >= self.cap {
            // Only at the wrap -- see the note on the command ring's `push`.
            self.enq = 0;
            self.sync_link_cycle_bit();
            self.cycle = !self.cycle;
        }
        Ok(phys)
    }

    /// Abandon this ring's pages without freeing them; see [`DmaBuf::leak`].
    fn leak(self) {
        self.buf.leak();
    }

    fn advance_dequeue(&mut self, n: usize) {
        for _ in 0..n {
            self.xfer_deq = (self.xfer_deq + 1) % self.cap;
        }
    }

    fn deq_phys(&self) -> u64 {
        (self.buf.phys + self.xfer_deq * 16) as u64
    }

    /// Data-buffer pointer of the TRB living at physical address `phys`, when
    /// that address is one of this ring's TRBs.
    ///
    /// A Transfer Event points at the TRB it completed, which is how we can
    /// tell which report buffer the controller just filled instead of trusting
    /// a software counter that has no way back once it slips.
    fn trb_buffer_at(&self, phys: u64) -> Option<u64> {
        let base = self.buf.phys as u64;
        let end = base + ((self.cap as u64) + 1) * 16;
        if phys < base || phys >= end || !(phys - base).is_multiple_of(16) {
            return None;
        }
        Some(self.buf.read_u64((phys - base) as usize))
    }

    /// The TRANSFER TRB physically before `phys` in this ring, wrapping past the
    /// Link TRB that closes the segment.
    ///
    /// The slot before the first one is the last DATA slot, `cap - 1`, and not
    /// the Link TRB that lives at `cap`. A Link TRB is not a transfer TRB and
    /// never completes: its pointer field holds this ring's own base address, so
    /// [`Self::trb_buffer_at`] answered that base as if it were a report buffer.
    /// The one caller compares that answer against the report buffer it is
    /// expecting, which means at every lap of the ring the comparison could not
    /// match and the dispatch head was resynced from the wrong TRB -- on a mouse
    /// reporting 125 times a second, a 64-TRB ring laps twice a second.
    fn prev_trb_phys(&self, phys: u64) -> u64 {
        let base = self.buf.phys as u64;
        if phys <= base {
            base + (self.cap.saturating_sub(1) as u64) * 16
        } else {
            phys - 16
        }
    }

    fn deq_cycle(&self) -> bool {
        if self.xfer_deq == self.enq {
            self.cycle
        } else {
            let off = self.xfer_deq * 16;
            let ctrl = self.buf.read_u32(off + 12);
            (ctrl & 1) != 0
        }
    }
}

struct EventRing {
    seg: DmaBuf,
    erst: DmaBuf,
    deq: usize,
    cycle: bool,
    ntrb: usize,
}

impl EventRing {
    fn new(n: usize) -> DeviceResult<Self> {
        let seg = DmaBuf::new(n * 16, 64)?;
        // Flush the event ring segment to physical memory so that:
        // 1. The xHC reads zeros (not stale garbage) for unwritten slots.
        // 2. Cache lines are clean so peek()'s clflush does not write dirty zeros
        //    back over DMA-written events on non-cache-coherent or partially-coherent
        //    environments (e.g. VMs with PCIe passthrough or IOMMU bypass).
        seg.flush(0, seg.len);
        let erst = DmaBuf::new(16, 64)?;
        erst.write_u64(0, seg.phys as u64);
        erst.write_u32(8, n as u32);
        erst.write_u32(12, 0);
        // Flush the ERST so the xHC can read it via DMA before the first event is posted.
        erst.flush(0, erst.len);
        Ok(Self {
            seg,
            erst,
            deq: 0,
            cycle: true,
            ntrb: n,
        })
    }

    fn peek(&self) -> Option<Trb> {
        let off = self.deq * 16;
        // El controlador xHCI escribe eventos en RAM via DMA.
        // En x86_64, aunque el hardware hace cache snooping en la mayoría de sistemas,
        // algunos entornos (IOMMU no coherente, VMs con passthrough parcial, etc.)
        // pueden dejar la línea de caché marcada como válida con los ceros originales.
        // USBSTS.EINT=1 pero 0 eventos visibles es la firma exacta de este problema.
        // Solución: invalidar la línea de caché del TRB actual antes de leerlo.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            // Cada TRB = 16 bytes. Una línea de caché = 64 bytes = 4 TRBs.
            // clflush invalida toda la línea, así que un solo flush es suficiente.
            _mm_clflush((self.seg.virt + off) as *const u8);
            // MFENCE, not LFENCE: CLFLUSH is ordered with respect to MFENCE
            // only (Intel SDM, CLFLUSH). An LFENCE here does not guarantee the
            // invalidate has completed before the load below, which is the
            // whole point of the flush.
            _mm_mfence();
        }
        let c = self.seg.read_u32(off + 12);
        if (c & 1) == (self.cycle as u32) {
            Some(Trb {
                p: self.seg.read_u64(off),
                status: self.seg.read_u32(off + 8),
                ctrl: c,
            })
        } else {
            None
        }
    }

    fn pop(&mut self) -> Option<Trb> {
        let t = self.peek()?;
        self.deq = (self.deq + 1) % self.ntrb;
        if self.deq == 0 {
            self.cycle = !self.cycle;
        }
        Some(t)
    }

    fn erdp_phys(&self) -> u64 {
        (self.seg.phys + self.deq * 16) as u64
    }

    fn erst_phys(&self) -> u64 {
        self.erst.phys as u64
    }
}

// ——— Velocidades (codigo de PORTSC.Port Speed, xHCI §5.4.8) ———

const SPEED_FULL: u8 = 1;
const SPEED_LOW: u8 = 2;
const SPEED_HIGH: u8 = 3;
const SPEED_SUPER: u8 = 4;

// ——— Hubs USB ———

const USB_CLASS_HUB: u8 = 0x09;
/// `bDescriptorType` del descriptor de hub (USB 2.0 §11.23.2.1) y de su gemelo
/// SuperSpeed (USB 3.2 §10.15.2.1).
const USB_DESC_HUB: u8 = 0x29;
const USB_DESC_SS_HUB: u8 = 0x2a;
/// Selectores de característica de la clase hub (USB 2.0 §11.24.2).
const HUB_FEAT_PORT_RESET: u16 = 4;
const HUB_FEAT_PORT_POWER: u16 = 8;
const HUB_FEAT_C_PORT_CONNECTION: u16 = 16;
const HUB_FEAT_C_PORT_ENABLE: u16 = 17;
const HUB_FEAT_C_PORT_SUSPEND: u16 = 18;
const HUB_FEAT_C_PORT_OVER_CURRENT: u16 = 19;
const HUB_FEAT_C_PORT_RESET: u16 = 20;
/// Solo USB 2.0 con LPM.
const HUB_FEAT_C_PORT_L1: u16 = 23;
/// Solo SuperSpeed.
const HUB_FEAT_C_PORT_LINK_STATE: u16 = 25;
const HUB_FEAT_C_PORT_CONFIG_ERROR: u16 = 26;
const HUB_FEAT_C_BH_PORT_RESET: u16 = 29;
/// Bits de `wPortStatus` (USB 2.0 §11.24.2.7.1).
const HUB_PORT_CONNECTION: u16 = 1 << 0;
const HUB_PORT_ENABLE: u16 = 1 << 1;
const HUB_PORT_RESET: u16 = 1 << 4;
const HUB_PORT_LOW_SPEED: u16 = 1 << 9;
const HUB_PORT_HIGH_SPEED: u16 = 1 << 10;
/// Bits de `wPortChange` de un hub USB 2.0 y la característica que limpia cada
/// uno (USB 2.0 §11.24.2.7.2, con el bit 5 de la ECN de LPM).
const HUB_PORT_CHANGES_USB2: [(u16, u16); 6] = [
    (1 << 0, HUB_FEAT_C_PORT_CONNECTION),
    (1 << 1, HUB_FEAT_C_PORT_ENABLE),
    (1 << 2, HUB_FEAT_C_PORT_SUSPEND),
    (1 << 3, HUB_FEAT_C_PORT_OVER_CURRENT),
    (1 << 4, HUB_FEAT_C_PORT_RESET),
    (1 << 5, HUB_FEAT_C_PORT_L1),
];

/// Y los de un hub SuperSpeed (USB 3.2 §10.16.2.6), que no son los mismos: los
/// bits 1 y 2 son reservados, el 5 es `C_BH_PORT_RESET` en vez de `C_PORT_L1`,
/// y existen dos más arriba que USB 2.0 no tiene.
const HUB_PORT_CHANGES_SS: [(u16, u16); 6] = [
    (1 << 0, HUB_FEAT_C_PORT_CONNECTION),
    (1 << 3, HUB_FEAT_C_PORT_OVER_CURRENT),
    (1 << 4, HUB_FEAT_C_PORT_RESET),
    (1 << 5, HUB_FEAT_C_BH_PORT_RESET),
    (1 << 6, HUB_FEAT_C_PORT_LINK_STATE),
    (1 << 7, HUB_FEAT_C_PORT_CONFIG_ERROR),
];

/// La tabla que le toca a un hub por su velocidad.
///
/// Con una sola tabla para los dos protocolos, un cambio se reconoce con la
/// característica de otro y el bit se queda puesto: el hub vuelve a señalar ese
/// puerto en cada informe de cambio de estado para siempre, y el driver lo
/// vuelve a leer para siempre. Es lo que pasaba con el bit 5 en un hub USB 2.0
/// (que es `C_PORT_L1`, no `C_BH_PORT_RESET`) y con los bits 6 y 7 de un hub
/// SuperSpeed, que no se limpiaban nunca.
fn hub_port_changes(hub_speed: u8) -> &'static [(u16, u16)] {
    if hub_speed >= SPEED_SUPER {
        &HUB_PORT_CHANGES_SS
    } else {
        &HUB_PORT_CHANGES_USB2
    }
}
/// Un route string lleva cinco niveles de cuatro bits (xHCI §8.9), así que un
/// dispositivo no puede estar a más de cinco hubs de la raíz.
const USB_MAX_TIERS: u8 = 5;
/// Y ningún hub tiene más puertos de los que un nibble puede nombrar.
const HUB_MAX_PORTS: u8 = 15;
/// Cada cuánto se barren los puertos de un hub cuyo endpoint de cambio de
/// estado no se pudo armar: ahí el sondeo es lo único que hay.
const HUB_SCAN_PERIOD_US: u64 = 1_000_000;
/// Y cada cuánto se barre uno que sí tiene su endpoint. Es una red de
/// seguridad --un informe perdido, un hub que no avisa-- y cada puerto cuesta
/// una transferencia de control, así que va mucho más espaciado.
const HUB_BACKSTOP_PERIOD_US: u64 = 5_000_000;
/// Tiempo máximo que se espera a que un puerto de hub salga del reset.
const HUB_RESET_TIMEOUT_US: u64 = 800_000;
/// `TRSTRCY`: recuperación tras el reset antes de hablarle al dispositivo
/// (USB 2.0 §7.1.7.5).
const HUB_RESET_RECOVERY_US: u64 = 10_000;

/// Dónde está un dispositivo en la topología USB, o sea todo lo que el Slot
/// Context de xHCI necesita para alcanzarlo (§6.2.2).
///
/// Un dispositivo en un puerto raíz tiene `route == 0` y no tiene padre; uno
/// detrás de un hub lleva en su route string el puerto aguas abajo de cada
/// nivel, cuatro bits por nivel y el nivel 1 en los bits 3:0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DevTopo {
    /// Puerto del hub raíz (base 1) del que cuelga todo el camino.
    root_port: u8,
    /// Route String: el campo de 20 bits del Slot Context DW0.
    route: u32,
    /// Slot del hub en el que está enchufado, 0 si está en un puerto raíz.
    parent_slot: u8,
    /// Puerto aguas abajo de ese hub (base 1), 0 si está en un puerto raíz.
    parent_port: u8,
    /// Slot del hub de alta velocidad cuyo Transaction Translator sirve a este
    /// dispositivo, y el puerto del que cuelga. Cero cuando no necesita
    /// ninguno: el dispositivo es HS o más rápido, o no hay un hub HS encima.
    tt_slot: u8,
    tt_port: u8,
    /// `true` cuando ese hub tiene un TT por puerto en vez de uno para todos.
    tt_multi: bool,
    /// Niveles entre este dispositivo y el puerto raíz: 0 en un puerto raíz.
    depth: u8,
}

impl DevTopo {
    fn root(port: u8) -> Self {
        Self {
            root_port: port,
            ..Self::default()
        }
    }

    /// La topología de un dispositivo enchufado al puerto `port` de este hub,
    /// que está en `self`, ocupa el slot `hub_slot` y corre a `hub_speed`,
    /// cuando el propio dispositivo ha enlazado a `speed`.
    ///
    /// `None` si el puerto no cabe en un nibble o si el dispositivo quedaría
    /// más allá del último nivel que un route string puede nombrar.
    fn child(
        &self,
        hub_slot: u8,
        hub_speed: u8,
        port: u8,
        speed: u8,
        hub_multi_tt: bool,
    ) -> Option<Self> {
        if self.depth >= USB_MAX_TIERS || port == 0 || port > HUB_MAX_PORTS {
            return None;
        }
        // Cada nivel se queda con un nibble, el nivel 1 en los bits 3:0.
        let route = self.route | ((port as u32) << (4 * self.depth as u32));
        let (tt_slot, tt_port, tt_multi) = if !matches!(speed, SPEED_FULL | SPEED_LOW) {
            // Solo se traduce lo que va a baja o plena velocidad.
            (0, 0, false)
        } else if hub_speed == SPEED_HIGH {
            // Este hub es el traductor.
            (hub_slot, port, hub_multi_tt)
        } else {
            // Un hub FS/LS colgado de uno HS: el traductor sigue siendo aquel.
            (self.tt_slot, self.tt_port, self.tt_multi)
        };
        Some(Self {
            root_port: self.root_port,
            route,
            parent_slot: hub_slot,
            parent_port: port,
            tt_slot,
            tt_port,
            tt_multi,
            depth: self.depth + 1,
        })
    }
}

/// Lo que hace falta saber de un hub ya enumerado.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HubInfo {
    ports: u8,
    /// TT Think Time tal cual va al Slot Context DW2 (bits 17:16).
    think_time: u8,
    /// Retardo entre encender un puerto y que responda, en ms.
    power_good_ms: u32,
    /// Siempre `false`: habilitar Multi-TT exige un `SET_INTERFACE` a la
    /// interfaz alternativa 1 del hub, que este driver no hace. Un TT único es
    /// válido en cualquier hub, así que no pasa nada por quedarse ahí.
    multi_tt: bool,
}

/// `bNbrPorts`, el think time y el retardo de encendido de un descriptor de hub
/// (USB 2.0 §11.23.2.1, USB 3.2 §10.15.2.1, que comparten los seis primeros
/// bytes).
fn parse_hub_descriptor(raw: &[u8]) -> Option<HubInfo> {
    if raw.len() < 6 {
        return None;
    }
    if (raw[0] as usize) < 6 || (raw[1] != USB_DESC_HUB && raw[1] != USB_DESC_SS_HUB) {
        return None;
    }
    let ports = raw[2];
    if ports == 0 {
        return None;
    }
    let chars = u16::from_le_bytes([raw[3], raw[4]]);
    Some(HubInfo {
        ports: ports.min(HUB_MAX_PORTS),
        think_time: ((chars >> 5) & 3) as u8,
        // `bPwrOn2PwrGood` va en unidades de 2 ms, y un hub que no pide nada
        // sigue necesitando los 100 ms que la especificación da a un puerto
        // para contestar después de encenderse.
        power_good_ms: ((raw[5] as u32) * 2).max(100),
        multi_tt: false,
    })
}

/// Bytes del mapa de bits de cambio de estado de un hub de `ports` puertos.
///
/// Es un bit por puerto más el bit 0, que es el del propio hub (USB 2.0
/// §11.12.4), redondeado a bytes.
fn hub_change_bytes(ports: u8) -> usize {
    (ports as usize + 1).div_ceil(8)
}

/// Los puertos que un mapa de bits de cambio de estado señala.
///
/// El bit `N` es el puerto `N`; el bit 0 es un cambio del hub entero, que este
/// driver no usa para nada todavía, así que no devuelve ningún puerto. Un bit
/// por encima de `ports` es basura o un hub que miente: se ignora en vez de
/// mandar un `GET_STATUS` a un puerto que no existe.
fn hub_changed_ports(bitmap: &[u8], ports: u8) -> Vec<u8> {
    (1..=ports)
        .filter(|&port| {
            bitmap
                .get(port as usize / 8)
                .is_some_and(|b| b & (1 << (port % 8)) != 0)
        })
        .collect()
}

/// El código de velocidad de PORTSC que corresponde a lo que el hub cuenta en
/// `wPortStatus`.
///
/// Un hub SuperSpeed solo tiene hijos SuperSpeed: los dispositivos USB 2.0 del
/// mismo conector físico cuelgan del hub USB 2.0 acompañante, que el
/// controlador enumera como otro dispositivo suyo.
fn hub_port_speed(hub_speed: u8, status: u16) -> u8 {
    if hub_speed >= SPEED_SUPER {
        hub_speed
    } else if status & HUB_PORT_LOW_SPEED != 0 {
        SPEED_LOW
    } else if status & HUB_PORT_HIGH_SPEED != 0 {
        SPEED_HIGH
    } else {
        SPEED_FULL
    }
}

const USB_CLASS_HID: u8 = 0x03;
const HID_SUBCLASS_BOOT: u8 = 0x01;
const HID_SUBCLASS_NONE: u8 = 0x00;
const HID_REQ_SET_PROTOCOL: u8 = 0x0b;
const HID_REQ_SET_IDLE: u8 = 0x0a;
const USB_DESC_IFACE: u8 = 0x04;
const USB_DESC_EP: u8 = 0x05;
const USB_DESC_HID: u8 = 0x21;
const USB_DESC_HID_REPORT: u8 = 0x22;
/// How much of a report descriptor `/proc/usbhid` keeps. A wheel mouse's
/// descriptor runs past 64 bytes (Microsoft's hi-res wheel sample is over
/// 120), and the wheel is declared near the end: a 64-byte snapshot cut off
/// exactly the part a dead wheel needs read.
const REPORT_DESC_SNAPSHOT: usize = 256;
const EP_TYPE_CONTROL: u32 = 4 << 3;
const EP_TYPE_INT_IN: u32 = 7 << 3;
const EP_TYPE_BULK_OUT: u32 = 2 << 3;
const EP_TYPE_BULK_IN: u32 = 6 << 3;

/// El Device Context Index de un `bEndpointAddress`: `2*N` para un endpoint
/// OUT, `2*N+1` para uno IN. Es la inversa de [`ep_addr_from_dci`].
///
/// `None` si no cabe en los 31 endpoints de un slot. El endpoint 0 no tiene
/// direccion: su DCI es 1 y lo pone `setup_device`.
fn dci_from_ep_addr(ep_addr: u8) -> Option<u8> {
    let num = ep_addr & 0x0f;
    if num == 0 {
        return None;
    }
    let dci = num * 2 + u8::from(ep_addr & 0x80 != 0);
    (dci < 32).then_some(dci)
}
const HID_PROTO_KEY: u8 = 1;
const HID_PROTO_MOUSE: u8 = 2;
const HID_PROTO_TABLET: u8 = 3;

/// `wValue` of SET_PROTOCOL (USB HID 1.11 §7.2.6).
const HID_PROTOCOL_BOOT: u8 = 0;
const HID_PROTOCOL_REPORT: u8 = 1;

/// True when a mouse interface is handing us reports too short for the layout
/// we parsed, in the one case where that is unambiguous: an interface with no
/// Report IDs, where every report on the endpoint *is* the mouse report. A
/// device that refused SET_PROTOCOL(Report) and stayed in boot protocol looks
/// exactly like this — three bytes where the layout wants four or more.
///
/// On an interface that multiplexes Report IDs a short report is simply a
/// different, shorter report (a consumer-control page, a battery report), so we
/// never guess there.
fn mouse_report_is_truncated(ml: &MouseLayout, actual_len: usize, boot_layout_ok: bool) -> bool {
    boot_layout_ok && ml.report_id.is_none() && actual_len < ml.report_bytes
}

/// Which HID protocol to put a boot-subclass interface in: the one whose report
/// layout we are going to decode.
///
/// Report protocol whenever the report descriptor gave us a layout for this
/// interface's role, because that layout describes the report-protocol report
/// — including its wheel, which the three-byte boot mouse report does not
/// have at all. Boot only as the fallback, where `dispatch_hid` decodes the
/// fixed boot layout and therefore needs the device to be in boot protocol.
fn hid_protocol_request(role: u8, parsed: &HidDescInfo) -> u8 {
    let have_layout = match role {
        HID_PROTO_MOUSE => !parsed.mouse.is_empty(),
        HID_PROTO_KEY => parsed.key.is_some(),
        _ => false,
    };
    if have_layout {
        HID_PROTOCOL_REPORT
    } else {
        HID_PROTOCOL_BOOT
    }
}
const TABLET_RANGE: i32 = 32767;
/// VirtualBox USB Tablet (`--mouse usbtablet`).
const VBOX_USB_TABLET_VID: u16 = 0x80ee;
const VBOX_USB_TABLET_PID: u16 = 0x0021;
/// QEMU emulated USB (`usb-kbd` / `usb-mouse` / `usb-tablet`). Product is
/// usually 0x0001; the tablet is the only one with `bInterfaceProtocol == 0`.
const QEMU_USB_VID: u16 = 0x0627;
const NO_MSI_VECTOR: usize = 0;
/// Largest interrupt-IN transfer we arm per TRB. HID reports are small, but a
/// HS interrupt endpoint may advertise up to 1024 bytes; each report buffer is
/// a full page, so this is a bound on the TRB length, not on memory.
const MAX_HID_TD: usize = 1024;

/// True only for the VM absolute pointers we parse as 16-bit ABS_X/Y.
///
/// A real-hardware HID interface with `bInterfaceProtocol == 0` ("None") is
/// *not* a tablet: it is the extra iface on almost every USB keyboard (media
/// keys) and many mice (vendor report). Treating those as tablets used to
/// raise [`USB_ABS_POINTER`], which then silenced the PS/2 aux path *and*
/// dropped boot-protocol USB mouse reports — keyboard still worked, the
/// pointer was dead on metal.
fn is_vm_abs_tablet(vid: u16, pid: u16, proto: u8) -> bool {
    if proto != 0 {
        return false;
    }
    (vid == VBOX_USB_TABLET_VID && pid == VBOX_USB_TABLET_PID) || vid == QEMU_USB_VID
}

/// Whether enumeration reads an interface's report descriptor: every
/// interface that has one, except a boot keyboard (decoded with the fixed boot
/// layout) and a VM tablet (decoded with its known VM layout).
///
/// A mouse is NOT an exception, even when `bInterfaceProtocol` already says
/// "mouse". Almost every real USB mouse is boot subclass with protocol 2, and
/// we used to classify it by those two fields alone and never read its
/// descriptor. With no layout, `hid_protocol_request` then asked for BOOT
/// protocol, whose report is three bytes with no wheel in it: the pointer
/// moved and the wheel was dead, on every real mouse, with nothing logged.
/// QEMU's `usb-mouse` hid it by sending its wheel byte in boot protocol too.
fn reads_report_descriptor(proto: u8, vid: u16, pid: u16, report_desc_len: u16) -> bool {
    report_desc_len > 0 && proto != HID_PROTO_KEY && !is_vm_abs_tablet(vid, pid, proto)
}

/// What an interface keeps from its parsed report descriptor. `class` is what
/// the descriptor classified as; `mouse_by_protocol` is true when the
/// interface's own protocol fields already made it a mouse.
fn iface_desc_info(class: HidClass, parsed: HidDescInfo, mouse_by_protocol: bool) -> HidDescInfo {
    HidDescInfo {
        // Only a mouse-classified interface gets a pointer layout; the
        // largest-report size matters for every interface's TD sizing.
        mouse: if class == HidClass::Mouse {
            parsed.mouse
        } else {
            MouseReports::default()
        },
        // The keyboard layout, on the other hand, is kept whatever the
        // interface classified as: a combo receiver classifies as Mouse (it
        // has relative X/Y) and still carries the keyboard reports, and so
        // does a gaming mouse with macro keys. On an interface that is a
        // mouse by protocol, though, a keyboard report WITHOUT a Report ID
        // would claim every report on the endpoint, the mouse's included, so
        // only one under its own ID is kept there.
        key: parsed
            .key
            .filter(|k| !mouse_by_protocol || k.report_id.is_some()),
        max_report_bytes: parsed.max_report_bytes,
    }
}

/// How to drive a protocol-0 HID interface after sniffing its report descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HidClass {
    Key,
    Mouse,
    Tablet,
    /// Consumer-control / vendor-only: do not bind (avoids fake pointer events).
    Skip,
    Unknown,
}

/// Walk a HID report descriptor looking for Generic Desktop X/Y (relative vs
/// absolute), a keyboard application collection, or consumer-control.
fn classify_hid_report(desc: &[u8]) -> HidClass {
    let mut usage_page: u32 = 0;
    let mut local_usages: [u32; 8] = [0; 8];
    let mut n_local: usize = 0;
    // One flag per application collection kind we care about, NOT "the last
    // one wins". A cheap USB keyboard (and every combo dongle) declares
    // `Keyboard` and then a `Consumer Control` collection for the media keys;
    // keeping only the last collection classified the whole interface as
    // `Skip`, so the keyboard was never bound and the device was dead.
    let mut app_keyboard = false;
    let mut app_mouse = false;
    let mut app_consumer = false;
    let mut saw_rel_x = false;
    let mut saw_rel_y = false;
    let mut saw_abs_x = false;
    let mut saw_abs_y = false;
    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];
        i += 1;
        if prefix == 0xfe {
            // Long item: [0xFE, size, tag, data…]
            if i + 2 > desc.len() {
                break;
            }
            let size = desc[i] as usize;
            i += 2 + size;
            continue;
        }
        let size = match prefix & 3 {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 4,
            _ => 0,
        };
        if i + size > desc.len() {
            break;
        }
        let mut data = 0u32;
        for b in 0..size {
            data |= (desc[i + b] as u32) << (8 * b);
        }
        i += size;
        let typ = (prefix >> 2) & 3;
        let tag = prefix >> 4;
        match (typ, tag) {
            (1, 0) => usage_page = data, // Global Usage Page
            (2, 0) => {
                // Local Usage
                if n_local < local_usages.len() {
                    local_usages[n_local] = data;
                    n_local += 1;
                }
            }
            (0, 0xa) => {
                // Collection
                if data == 0x01 && n_local > 0 {
                    match (usage_page, local_usages[0]) {
                        (0x01, 0x06) => app_keyboard = true,
                        (0x01, 0x02) | (0x01, 0x01) => app_mouse = true,
                        (0x0c, _) => app_consumer = true,
                        _ => {}
                    }
                }
                n_local = 0;
            }
            (0, 8) => {
                // Input
                let relative = (data & 0x4) != 0;
                for u in local_usages.iter().take(n_local).copied() {
                    if usage_page == 0x01 {
                        if u == 0x30 {
                            if relative {
                                saw_rel_x = true;
                            } else {
                                saw_abs_x = true;
                            }
                        }
                        if u == 0x31 {
                            if relative {
                                saw_rel_y = true;
                            } else {
                                saw_abs_y = true;
                            }
                        }
                    }
                }
                n_local = 0;
            }
            (0, _) => n_local = 0, // other Main items clear locals
            _ => {}
        }
    }
    if saw_rel_x && saw_rel_y {
        return HidClass::Mouse;
    }
    if saw_abs_x && saw_abs_y {
        return HidClass::Tablet;
    }
    if app_keyboard {
        return HidClass::Key;
    }
    if app_mouse {
        return HidClass::Mouse;
    }
    // Consumer-control only, with no pointer and no keyboard anywhere in the
    // descriptor: nothing we can drive, and binding it would fabricate
    // pointer events.
    if app_consumer {
        return HidClass::Skip;
    }
    HidClass::Unknown
}

/// A field inside a HID input report: bit offset from the start of the report
/// (the Report ID byte included, when there is one) and width in bits. HID
/// packs fields LSB-first and does NOT byte-align them — every Logitech
/// Unifying mouse reports 12-bit axes — so decoding has to be bit-granular.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BitField {
    off: usize,
    len: usize,
}

/// The pointer fields one input report carries, extracted from its HID report
/// descriptor. Any of them can be missing: a mouse may split itself across
/// report IDs (see [`MouseReports`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MouseLayout {
    report_id: Option<u8>,
    /// Button bitmap (bit0 = left, bit1 = right, bit2 = middle, …), ≤ 8 bits.
    /// Only a block that starts at Button 1 counts: a second block of extra
    /// buttons in another report is not left/right/middle.
    buttons: Option<BitField>,
    x: Option<BitField>,
    y: Option<BitField>,
    /// Vertical wheel (Generic Desktop usage 0x38), if present.
    wheel: Option<BitField>,
    /// Horizontal pan (Consumer usage 0x238 "AC Pan"), if present.
    hwheel: Option<BitField>,
    /// Size in bytes of this report, ID byte included.
    report_bytes: usize,
}

/// How many input reports of one interface can carry pointer fields.
const MAX_MOUSE_REPORTS: usize = 4;

/// Every input report of an interface that carries a pointer field.
///
/// Linux maps each field of each report on its own, so a mouse is free to put
/// its buttons and wheel in one report ID and its X/Y in another -- Xiaomi's
/// wireless mouse dongle does exactly that (`MIDongleMIWirelessMouse` in the
/// kernel's HID selftests). Requiring one report with buttons, X and Y found
/// no mouse at all there, which on a boot-subclass interface meant falling
/// back to boot protocol: pointer alive, wheel dead, and nothing logged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MouseReports {
    reports: [Option<MouseLayout>; MAX_MOUSE_REPORTS],
}

impl MouseReports {
    fn is_empty(&self) -> bool {
        self.reports[0].is_none()
    }

    fn iter(&self) -> impl Iterator<Item = MouseLayout> + '_ {
        self.reports.iter().map_while(|r| *r)
    }

    /// The report that moves the pointer: the first one with X and Y.
    fn primary(&self) -> Option<MouseLayout> {
        self.iter().find(|r| r.x.is_some() && r.y.is_some())
    }

    /// A vertical wheel somewhere. A pan alone does not scroll a page.
    fn has_wheel(&self) -> bool {
        self.iter().any(|r| r.wheel.is_some())
    }

    /// The layout of the report in `buf`: by its Report ID, or the only
    /// report of an interface that has none.
    fn for_report(&self, buf: &[u8]) -> Option<MouseLayout> {
        self.iter().find(|r| match r.report_id {
            Some(id) => buf.first().copied() == Some(id),
            None => true,
        })
    }
}

/// Layout of a keyboard input report, extracted from its HID report
/// descriptor. The boot report is one particular instance of this
/// ([`BOOT_KEY_LAYOUT`]), not a separate case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyLayout {
    report_id: Option<u8>,
    /// The modifier bitmap (Keyboard page usages 0xE0..=0xE7, Variable),
    /// ≤ 8 bits.
    mods: BitField,
    /// The keycode array. `off` is the first entry, `len` the width of ONE
    /// entry; there are `key_count` of them back to back.
    keys: BitField,
    key_count: usize,
    /// Size in bytes of this report, ID byte included.
    report_bytes: usize,
}

/// The USB HID boot keyboard report: `[modifiers, reserved, k0..k5]`.
const BOOT_KEY_LAYOUT: KeyLayout = KeyLayout {
    report_id: None,
    mods: BitField { off: 0, len: 8 },
    keys: BitField { off: 16, len: 8 },
    key_count: 6,
    report_bytes: 8,
};

/// Pull the modifier bitmap and up to six keycodes out of one report.
///
/// This replaces the old "`tmp[0]` is modifiers, `tmp[2..8]` are the keys"
/// assumption, which is true only for a boot-protocol report. A keyboard that
/// speaks report protocol puts its Report ID in `tmp[0]`, so the ID was
/// decoded as a modifier bitmap: pressing any key on such a board latched
/// phantom Ctrl/Shift/Alt that were never released.
fn decode_keyboard(buf: &[u8], kl: &KeyLayout) -> (u8, [u8; 6]) {
    let mods = read_bits(buf, kl.mods.off, kl.mods.len.min(8)) as u8;
    let mut keys = [0u8; 6];
    let width = kl.keys.len.clamp(1, 8);
    for (j, slot) in keys.iter_mut().enumerate().take(kl.key_count.min(6)) {
        *slot = read_bits(buf, kl.keys.off + j * kl.keys.len, width) as u8;
    }
    (mods, keys)
}

/// What a HID report descriptor tells us about an interface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HidDescInfo {
    /// The reports that carry pointer fields; empty when the interface is not
    /// a relative mouse.
    mouse: MouseReports,
    /// The first keyboard report on the interface, if any. An interface can
    /// carry BOTH this and `mouse` under different report IDs — that is what
    /// every wireless combo receiver looks like.
    key: Option<KeyLayout>,
    /// Size in bytes of the LARGEST input report on the interface, any report
    /// ID. Interrupt-IN transfers must be armed at least this large: a longer
    /// non-mouse report sharing the endpoint (Logitech HID++ is 20 bytes on
    /// the mouse interface) would otherwise overrun the TD into a Babble
    /// error and halt the endpoint for good.
    max_report_bytes: usize,
}

/// Read `len` bits starting at bit `off` (LSB-first, as HID packs fields).
/// Bits past the end of `buf` read as zero.
fn read_bits(buf: &[u8], off: usize, len: usize) -> u32 {
    let mut v = 0u32;
    for i in 0..len.min(32) {
        let bit = off + i;
        let Some(&byte) = buf.get(bit / 8) else {
            break;
        };
        if (byte >> (bit % 8)) & 1 != 0 {
            v |= 1 << i;
        }
    }
    v
}

/// Read a two's-complement field of `len` bits (1..=32).
fn read_signed_bits(buf: &[u8], off: usize, len: usize) -> i32 {
    if len == 0 || len > 32 {
        return 0;
    }
    let raw = read_bits(buf, off, len);
    if len == 32 {
        return raw as i32;
    }
    if raw & (1 << (len - 1)) != 0 {
        (raw | (u32::MAX << len)) as i32
    } else {
        raw as i32
    }
}

/// One wheel detent in the high-resolution scroll units both Linux and Windows
/// agreed on: `REL_WHEEL_HI_RES` counts 120 per notch, so a wheel that can
/// report fractions of a notch has somewhere to put them.
const HI_RES_PER_DETENT: i32 = 120;

/// Emit the scroll axes of one HID report the way `hidinput_handle_scroll`
/// does: the low-resolution axis carries the raw field value and the
/// high-resolution one carries 120 times it.
///
/// Two things here were wrong before and both are visible as "the wheel does
/// not work":
///
/// * The sign. A HID `Wheel` (Generic Desktop usage 0x38) is positive when the
///   wheel turns away from the user, which is exactly `REL_WHEEL` positive;
///   `hid-input.c` passes the value straight through. We were negating it, so
///   every scroll went the wrong way — and a desktop that scrolls backwards is
///   reported as one that does not scroll. (The negation is correct for the
///   PS/2 IntelliMouse byte, where it came from; USB is not PS/2.)
/// * The missing high-resolution axis. Linux has emitted `REL_WHEEL_HI_RES`
///   for every HID mouse since 5.0 and libinput's wheel state machine works in
///   those units, synthesising them from the low-resolution axis only when the
///   device does not advertise them. Emitting both is what a real mouse looks
///   like.
fn emit_scroll(lis: &EventListener<InputEvent>, wheel: i32, hwheel: i32) {
    for (value, lo, hi) in [
        (wheel, REL_WHEEL, REL_WHEEL_HI_RES),
        (hwheel, REL_HWHEEL, REL_HWHEEL_HI_RES),
    ] {
        if value == 0 {
            continue;
        }
        lis.trigger(InputEvent {
            event_type: InputEventType::RelAxis,
            code: lo,
            value,
        });
        lis.trigger(InputEvent {
            event_type: InputEventType::RelAxis,
            code: hi,
            value: value.saturating_mul(HI_RES_PER_DETENT),
        });
    }
}

/// One relative-mouse report, decoded into what evdev wants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MouseDelta {
    /// Button bitmap, bit 0 = left, as the report carries it; `None` when
    /// this report has no buttons, so the ones held stay held.
    buttons: Option<u8>,
    dx: i32,
    dy: i32,
    wheel: i32,
    hwheel: i32,
}

/// Pull one relative-mouse report apart.
///
/// `layout` is the report-descriptor layout when we parsed one and the device
/// is actually speaking that protocol; `None` falls back to the fixed boot
/// layout `[buttons, dx, dy, wheel, pan]`, which is only meaningful on an
/// interface bound as a mouse that really speaks boot protocol -- that is what
/// `boot_layout_ok` carries. A report-protocol interface whose descriptor we
/// could not parse decodes to `None` rather than to garbage: its report ID
/// would come out as a stuck button.
///
/// Returns `None` when this report is not this mouse's: a shared interface
/// multiplexes report IDs, and the keyboard side of a combo receiver must not
/// have its keycodes decoded as buttons and deltas.
fn decode_mouse_report(
    layouts: &MouseReports,
    buf: &[u8],
    report_len: usize,
    boot_layout_ok: bool,
) -> Option<MouseDelta> {
    if report_len < 3 || buf.len() < 3 {
        return None;
    }
    if !layouts.is_empty() {
        let ml = layouts.for_report(buf)?;
        let signed =
            |f: Option<BitField>| f.map(|f| read_signed_bits(buf, f.off, f.len)).unwrap_or(0);
        return Some(MouseDelta {
            buttons: ml.buttons.map(|f| read_bits(buf, f.off, f.len) as u8),
            dx: signed(ml.x),
            dy: signed(ml.y),
            wheel: signed(ml.wheel),
            hwheel: signed(ml.hwheel),
        });
    }
    match boot_layout_ok {
        true => Some(MouseDelta {
            buttons: Some(buf[0]),
            dx: buf[1] as i8 as i32,
            dy: buf[2] as i8 as i32,
            wheel: if report_len >= 4 {
                buf[3] as i8 as i32
            } else {
                0
            },
            hwheel: if report_len >= 5 {
                buf[4] as i8 as i32
            } else {
                0
            },
        }),
        false => None,
    }
}

/// Turn one decoded report into the evdev frame for it, and return the button
/// bitmap to remember. Buttons are edges, axes are deltas, and the frame is
/// closed by exactly one `SYN_REPORT` however little moved.
fn emit_mouse(lis: &EventListener<InputEvent>, d: MouseDelta, last_buttons: u8) -> u8 {
    let buttons = d.buttons.unwrap_or(last_buttons);
    for (mask, code) in [
        (1u8, BTN_LEFT),
        (2u8, BTN_RIGHT),
        (4u8, BTN_MIDDLE),
        (8u8, BTN_SIDE),
        (16u8, BTN_EXTRA),
    ] {
        let down = (buttons & mask) != 0;
        let was = (last_buttons & mask) != 0;
        if down != was {
            lis.trigger(InputEvent {
                event_type: InputEventType::Key,
                code,
                value: if down { 1 } else { 0 },
            });
        }
    }
    if d.dx != 0 {
        lis.trigger(InputEvent {
            event_type: InputEventType::RelAxis,
            code: REL_X,
            value: d.dx,
        });
    }
    if d.dy != 0 {
        lis.trigger(InputEvent {
            event_type: InputEventType::RelAxis,
            code: REL_Y,
            // USB HID reports Y as down-positive, exactly the evdev REL_Y
            // convention libinput expects -- emit as-is. (The earlier `-dy`
            // was copied from the PS/2 driver, where +Y means up; under
            // libinput it inverted the axis.)
            value: d.dy,
        });
    }
    emit_scroll(lis, d.wheel, d.hwheel);
    lis.trigger(InputEvent {
        event_type: InputEventType::Syn,
        code: SYN_REPORT,
        value: 0,
    });
    buttons
}

/// Linux's own ceilings on a HID report descriptor (`hid_parser_global` in
/// `hid-core.c`): a single field is at most 256 bits wide and one report
/// carries at most 12288 usages.
///
/// Without them a malformed (or hostile) descriptor that declares
/// `Report Count = 0xffffffff` makes the per-field walk below run for four
/// billion iterations inside the xHCI interrupt handler: the machine simply
/// stops booting, with no message, at the moment a USB device is enumerated.
const HID_MAX_REPORT_SIZE_BITS: u32 = 256;
const HID_MAX_USAGES: u32 = 12288;
/// Ceiling on the running bit position of one report. 16 KiB of report is far
/// past anything real (the largest we ever arm is [`MAX_HID_TD`]) and keeps
/// `bit_pos` from wrapping into a nonsense `max_report_bytes`.
const HID_MAX_REPORT_BITS: usize = 16 * 1024 * 8;

/// Full 32-bit HID usage: usage page in the high half, usage id in the low.
const USAGE_GD_X: u32 = 0x0001_0030;
const USAGE_GD_Y: u32 = 0x0001_0031;
const USAGE_GD_WHEEL: u32 = 0x0001_0038;
const USAGE_CONSUMER_AC_PAN: u32 = 0x000C_0238;
const USAGE_PAGE_BUTTON: u32 = 0x09;
const USAGE_PAGE_KEYBOARD: u32 = 0x07;
/// Keyboard page usage 0xE0 (Left Control), the first of the eight modifiers.
const USAGE_KEY_LEFTCTRL: u32 = 0x0007_00E0;

/// Parse a HID report descriptor: find the relative-mouse report (report ID,
/// button block, X/Y/wheel/pan fields as bit positions) and the size of the
/// largest input report on the interface.
///
/// This is what lets a report-protocol mouse (bInterfaceSubClass 0, so no boot
/// protocol) work: its report is NOT the fixed boot `[buttons, dx, dy]`. Real
/// interfaces carry several report IDs (mouse + consumer control + vendor
/// HID++), 12- or 16-bit axes, more than 8 buttons and a pan wheel. We walk
/// every Input item tracking the running bit position per report ID; the
/// first report that has buttons, a relative X and a relative Y becomes the
/// mouse layout, and every report's size feeds `max_report_bytes`.
///
/// A report ID can be interrupted and come back: Microsoft's hi-res wheel
/// sample declares X/Y under the input report's ID, switches to a Feature
/// report ID for the Resolution Multiplier, then switches BACK to declare the
/// Wheel. Those fields continue the same input report (Linux's hid-core
/// appends them to it), so each ID keeps its own walk and resumes it.
fn parse_hid_descriptor(desc: &[u8]) -> HidDescInfo {
    // Global items (saved/restored by Push/Pop). Report ID is a global item
    // too (HID 1.11 §6.2.2.7), so the stack carries it; it lives in `cur.id`.
    let mut usage_page: u32 = 0;
    let mut report_size: u32 = 0;
    let mut report_count: u32 = 0;
    let mut stack: [(u32, u32, u32, Option<u8>); 8] = [(0, 0, 0, None); 8];
    let mut sp = 0usize;
    // Local items (cleared by every Main item). Usages are normalised to
    // `(page << 16) | id` so 4-byte extended usages and 2-byte ones compare
    // alike.
    let mut local_usages: [u32; 16] = [0; 16];
    let mut n_local = 0usize;
    let mut usage_min: u32 = 0;
    let mut usage_max: u32 = 0;
    /// One input report as walked so far.
    #[derive(Clone, Copy, Default)]
    struct ReportWalk {
        id: Option<u8>,
        bit_pos: usize,
        buttons: Option<BitField>,
        x: Option<BitField>,
        y: Option<BitField>,
        wheel: Option<BitField>,
        hwheel: Option<BitField>,
        kmods: Option<BitField>,
        kkeys: Option<(BitField, usize)>,
    }
    let mut cur = ReportWalk::default();
    // Every report left by a Report ID switch, in order of first appearance,
    // so one that comes back resumes where it stopped.
    let mut parked: Vec<ReportWalk> = Vec::new();
    fn park(parked: &mut Vec<ReportWalk>, r: ReportWalk) {
        match parked.iter_mut().find(|p| p.id == r.id) {
            Some(slot) => *slot = r,
            None => parked.push(r),
        }
    }
    /// Make `id` the report being walked: park the current one, and resume
    /// `id`'s walk if it was interrupted earlier (see the doc comment above).
    /// A report with an ID starts at bit 8, after the ID byte.
    fn switch_to(cur: &mut ReportWalk, parked: &mut Vec<ReportWalk>, id: Option<u8>) {
        if cur.id == id {
            return;
        }
        if cur.id.is_some() || cur.bit_pos > 0 {
            park(parked, *cur);
        }
        *cur = parked
            .iter()
            .find(|p| p.id == id)
            .copied()
            .unwrap_or(ReportWalk {
                id,
                bit_pos: if id.is_some() { 8 } else { 0 },
                ..ReportWalk::default()
            });
    }

    let mut info = HidDescInfo::default();

    /// Close a walked report: account its size, and keep it if it carries a
    /// pointer field or is the first keyboard report.
    fn finish(info: &mut HidDescInfo, r: &ReportWalk) {
        let bytes = r.bit_pos.div_ceil(8);
        info.max_report_bytes = info.max_report_bytes.max(bytes);
        if info.key.is_none() {
            if let (Some(mods), Some((keys, key_count))) = (r.kmods, r.kkeys) {
                info.key = Some(KeyLayout {
                    report_id: r.id,
                    mods,
                    keys,
                    key_count,
                    report_bytes: bytes,
                });
            }
        }
        let pointer = r.buttons.is_some()
            || r.x.is_some()
            || r.y.is_some()
            || r.wheel.is_some()
            || r.hwheel.is_some();
        if pointer {
            if let Some(slot) = info.mouse.reports.iter_mut().find(|s| s.is_none()) {
                *slot = Some(MouseLayout {
                    report_id: r.id,
                    buttons: r.buttons,
                    x: r.x,
                    y: r.y,
                    wheel: r.wheel,
                    hwheel: r.hwheel,
                    report_bytes: bytes,
                });
            }
        }
    }

    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];
        i += 1;
        if prefix == 0xfe {
            // Long item: [0xFE, size, tag, data…]
            if i + 2 > desc.len() {
                break;
            }
            let size = desc[i] as usize;
            i += 2 + size;
            continue;
        }
        let size = match prefix & 3 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        if i + size > desc.len() {
            break;
        }
        let mut data = 0u32;
        for b in 0..size {
            data |= (desc[i + b] as u32) << (8 * b);
        }
        i += size;
        let typ = (prefix >> 2) & 3;
        let tag = prefix >> 4;
        // A usage item of 4 bytes carries its own page in the high half.
        let full_usage = |d: u32| if size == 4 { d } else { (usage_page << 16) | d };
        match (typ, tag) {
            (1, 0) => usage_page = data, // Global Usage Page
            // Global Report Size / Report Count, clamped (see the constants).
            (1, 7) => report_size = data.min(HID_MAX_REPORT_SIZE_BITS),
            (1, 9) => report_count = data.min(HID_MAX_USAGES),
            (1, 0xa) => {
                // Push
                if sp < stack.len() {
                    stack[sp] = (usage_page, report_size, report_count, cur.id);
                    sp += 1;
                }
            }
            (1, 0xb) => {
                // Pop
                if sp > 0 {
                    sp -= 1;
                    let id;
                    (usage_page, report_size, report_count, id) = stack[sp];
                    // Popping back to another report ID moves the walk
                    // there, exactly as a Report ID item would.
                    switch_to(&mut cur, &mut parked, id);
                }
            }
            (1, 8) => {
                // Global Report ID: every report on the interface starts with
                // its ID byte, so a new report's fields begin at bit 8.
                switch_to(&mut cur, &mut parked, Some(data as u8));
            }
            (2, 0) => {
                if n_local < local_usages.len() {
                    local_usages[n_local] = full_usage(data);
                    n_local += 1;
                }
            }
            (2, 1) => usage_min = full_usage(data),
            (2, 2) => usage_max = full_usage(data),
            (0, 8) => {
                // Input main item. data: bit0 Constant, bit1 Variable,
                // bit2 Relative.
                let constant = (data & 0x1) != 0;
                let variable = (data & 0x2) != 0;
                let relative = (data & 0x4) != 0;
                let fbits = report_size as usize;
                let count = report_count as usize;
                if !constant && fbits > 0 {
                    if usage_page == USAGE_PAGE_KEYBOARD {
                        // A keyboard report is two items on the Keyboard page:
                        // a Variable bitmap of the eight modifiers (Usage
                        // Minimum 0xE0), and an Array of keycodes. Recognising
                        // them by shape rather than assuming the boot layout is
                        // what lets a report-protocol keyboard work at all.
                        if variable && fbits == 1 && usage_min == USAGE_KEY_LEFTCTRL {
                            cur.kmods.get_or_insert(BitField {
                                off: cur.bit_pos,
                                len: count.min(8),
                            });
                        } else if !variable && count > 0 {
                            cur.kkeys.get_or_insert((
                                BitField {
                                    off: cur.bit_pos,
                                    len: fbits,
                                },
                                count,
                            ));
                        }
                    } else if usage_page == USAGE_PAGE_BUTTON {
                        // Button block: one bit per button. Only the first 8
                        // map onto BTN_LEFT..BTN_EXTRA; the rest are skipped
                        // but still counted in `bit_pos`. The block has to
                        // start at Button 1 for its bits to be left, right,
                        // middle: a mouse that sends buttons 6..16 in a
                        // report of their own must not have them read as
                        // left and right.
                        let first = if usage_min != 0 {
                            usage_min
                        } else if n_local > 0 {
                            local_usages[0]
                        } else {
                            1
                        };
                        if fbits == 1 && cur.buttons.is_none() && first & 0xffff == 1 {
                            cur.buttons = Some(BitField {
                                off: cur.bit_pos,
                                len: count.min(8),
                            });
                        }
                    } else {
                        let from_range = usage_min != 0 && usage_max >= usage_min;
                        for j in 0..count {
                            // HID 1.11 §6.2.2.8: usages apply to fields in
                            // order; the last listed usage covers the rest. A
                            // usage-less item has no usage at all.
                            let usage = if j < n_local {
                                local_usages[j]
                            } else if from_range {
                                (usage_min + j as u32).min(usage_max)
                            } else if n_local > 0 {
                                local_usages[n_local - 1]
                            } else {
                                0
                            };
                            let f = BitField {
                                off: cur.bit_pos + j * fbits,
                                len: fbits,
                            };
                            match usage {
                                USAGE_GD_X if relative => {
                                    cur.x.get_or_insert(f);
                                }
                                USAGE_GD_Y if relative => {
                                    cur.y.get_or_insert(f);
                                }
                                USAGE_GD_WHEEL if relative => {
                                    cur.wheel.get_or_insert(f);
                                }
                                USAGE_CONSUMER_AC_PAN if relative => {
                                    cur.hwheel.get_or_insert(f);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                cur.bit_pos = cur
                    .bit_pos
                    .saturating_add(fbits.saturating_mul(count))
                    .min(HID_MAX_REPORT_BITS);
                n_local = 0;
                usage_min = 0;
                usage_max = 0;
            }
            (0, _) => {
                // Any other Main item (Collection/Output/Feature/…) clears the
                // locals. Output/Feature fields do not occupy input bits.
                n_local = 0;
                usage_min = 0;
                usage_max = 0;
            }
            _ => {}
        }
    }
    park(&mut parked, cur);
    for r in &parked {
        finish(&mut info, r);
    }
    // A relative mouse is a report with X and Y plus buttons somewhere, in
    // that report or another. Axes wider than the decoder are not a mouse we
    // can drive.
    let bad = |f: Option<BitField>| f.is_none_or(|f| f.len == 0 || f.len > 32);
    let drivable = info.mouse.primary().is_some_and(|p| !bad(p.x) && !bad(p.y))
        && info
            .mouse
            .iter()
            .any(|r| r.buttons.is_some_and(|b| b.len > 0));
    if !drivable {
        info.mouse = MouseReports::default();
    }
    info
}

/// Set when a USB HID tablet (QEMU `usb-tablet`, VirtualBox USB Tablet) is
/// enumerated. Those devices report *absolute* coordinates; a PS/2 mouse on
/// the same VM still delivers relative packets and would fight the tablet
/// (jumps, doubled motion). The PS/2 aux path checks this and stays quiet.
static USB_ABS_POINTER: AtomicBool = AtomicBool::new(false);

/// True once an absolute USB pointer has been enumerated.
pub fn usb_abs_pointer_active() -> bool {
    USB_ABS_POINTER.load(Ordering::Relaxed)
}

/// Number of TRBs (and report buffers) kept queued on every interrupt-IN HID
/// endpoint. A single in-flight TRB is fragile: if one transfer-completion
/// event is ever missed (event-ring race, lost MSI, idle-induced timer stall)
/// the endpoint goes silent forever — that's the "input dies after a few
/// seconds" symptom on real HW. Keeping a small ring of TRBs (each pointing at
/// its own buffer) means the controller can still satisfy several more
/// transfers before silence, and the resubmit on every event keeps the depth
/// constant. Buffers are tiny (≤ 64 B each) so the cost is negligible.
const HID_QUEUE_DEPTH: usize = 4;

/// Soft-recovery policy for an HCHalted controller. The driver used to set
/// a single global halted latch and short-circuit `poll()` forever after, meaning a
/// single transient bus error killed input until reboot. Instead, the first
/// few times we observe HCHalted we now try a soft restart (clear sticky
/// USBSTS errors, write RS=1, wait briefly for HCH to drop). If that
/// recovers the controller, attempts reset; if every attempt fails we latch
/// only that controller as dead. Backoff stops the recovery from
/// hammering the controller on every io-wait iteration.
const MAX_HALT_RECOVERY_ATTEMPTS: u8 = 8;
const HALT_RECOVERY_BACKOFF_US: u64 = 500_000;
const HALT_RECOVERY_WAIT_US: u64 = 50_000;

pub struct XhciInner {
    pub mmio: XhciMmio,
    cmd: CmdRing,
    ev: EventRing,
    dcbaa: DmaBuf,
    pub max_slots: u8,
    pub max_ports: u8,
    context_size: usize,
    pub msi_vector: usize,
    slot_speed: Vec<u8>,
    /// Topología de cada slot direccionado, indexada por slot id. `None` =
    /// slot libre. Sustituye al antiguo `slot_port`: el puerto raíz es solo uno
    /// de los campos, y detrás de un hub no identifica un dispositivo.
    slot_topo: Vec<Option<DevTopo>>,
    dev_ctx: Vec<Option<DmaBuf>>,
    xfer_rings: Vec<Option<XferRing>>,
    scratch_tbl: Option<DmaBuf>,
    scratch_pages: Vec<DmaBuf>,
    hids: Vec<HidDev>,
    /// Cambios de puerto diferidos para evitar re-entrada recursiva en pop_ev.
    pending_port_changes: Vec<u8>,
    /// Intentos consecutivos de enumeracion fallidos por puerto (indexado por
    /// `port_id`). Acota el reintento que abre `handle_port_status_change`
    /// cuando el puerto sigue conectado sin slot: sin esta cota, un puerto con
    /// un dispositivo que no enumera nunca se reintentaria en cada tick,
    /// varios segundos cada vez.
    port_enum_fails: Vec<u8>,
    /// HID interrupt endpoints (slot, dci) that completed a transfer with an
    /// error (Stall/Babble/…) and need a Reset Endpoint + re-arm. Deferred for
    /// the same reason as `pending_port_changes`: the recovery issues commands
    /// and waits on the event ring, which must not run nested inside `pop_ev`.
    /// `(slot, dci, stalled)` for endpoints that errored during an event
    /// drain. `stalled` means the completion code was Stall Error, so the
    /// DEVICE also has to be told to clear its halt.
    pending_ep_resets: Vec<(u8, u8, bool)>,
    /// Todos los dispositivos enumerados, tengan driver o no.
    devs: Vec<UsbDev>,
    /// Unidades de almacenamiento masivo que respondieron.
    mscs: Vec<MscDev>,
    /// Unidades que ya contestaron su capacidad y estan por dar de alta como
    /// disco, y discos dados de alta que estan por dar de baja.
    ///
    /// Las dos cosas se hacen FUERA del cerrojo de este controlador: dar de
    /// alta toca las listas de `kernel-hal`, y encadenar ese cerrojo con este
    /// es como se montan los ciclos que ya han parado esta maquina. Asi que
    /// quien descubre la unidad solo la apunta aqui, y `drain_disk_changes` la
    /// recoge cuando no tiene nada cogido.
    pending_disk_regs: Vec<u64>,
    pending_disk_unregs: Vec<Arc<UsbDisk>>,
    /// Hubs ya configurados, en el orden en que se enumeraron.
    hubs: Vec<HubDev>,
    /// `(slot del hub, puerto)` que un endpoint de cambio de estado ha
    /// señalado. Diferidos por la misma razón que `pending_port_changes`: la
    /// enumeración emite comandos y espera en el anillo de eventos, y eso no
    /// puede correr anidado dentro de `pop_ev`.
    pending_hub_ports: Vec<(u8, u8)>,
    /// HID enumeration deferred from PCI probe so boot can pass 80% quickly.
    boot_enum_pending: bool,
    /// Number of consecutive soft-recovery attempts since the controller last
    /// responded normally. Reset to 0 on every successful, non-halted poll.
    halt_attempts: u8,
    /// Monotonic timestamp (µs) of the last soft-recovery attempt; used to
    /// back off so we don't bang on the controller every io-wait iteration.
    halt_last_attempt_us: u64,
}

/// Una unidad de almacenamiento masivo que hablo con nosotros.
struct MscDev {
    slot: u8,
    iface: u8,
    /// DCI de sus dos endpoints bulk.
    dci_in: u8,
    dci_out: u8,
    max_lun: u8,
    inquiry: ScsiInquiry,
    /// `None` cuando la unidad contesto el INQUIRY pero no la capacidad: un
    /// lector de tarjetas sin tarjeta es exactamente eso.
    capacity: Option<ScsiCapacity>,
    /// Etiqueta de la siguiente orden. Cada una tiene que llevar la suya.
    next_tag: u32,
    /// Identidad estable de ESTA unidad, que no es su `slot`.
    ///
    /// Los slots se reutilizan: desenchufar un pendrive y enchufar otro en el
    /// mismo puerto puede devolver el mismo numero de slot. Un `UsbDisk` que
    /// guardase el slot leeria del disco nuevo creyendo que es el viejo, con
    /// el cache de bloques del sistema de archivos del viejo encima. Asi que lo
    /// que guarda es este contador, que no se reutiliza nunca.
    disk_id: u64,
    /// El disco de bloques dado de alta, mientras lo este. `None` en una unidad
    /// sin medio dentro (un lector de tarjetas vacio) o que no llego a
    /// contestar su capacidad.
    disk: Option<Arc<UsbDisk>>,
    /// Bufer de rebote para las fases de datos, creado en el primer acceso.
    ///
    /// Lo que entrega el sistema de archivos es un `&[u8]` cualquiera, que no
    /// tiene por que ser contiguo en fisico ni estar donde el controlador pueda
    /// llegar, asi que todo pasa por aqui. Vive en la unidad y no en cada
    /// llamada porque pedirselo al asignador de DMA en cada lectura de disco es
    /// lo contrario de lo que hace un camino de E/S.
    bounce: Option<DmaBuf>,
}

/// Cuanto se espera por una transferencia bulk. Una unidad que acaba de
/// arrancar puede tardar segundos en contestar su primer TEST UNIT READY.
const BULK_TIMEOUT_US: u64 = 5_000_000;
/// Intentos de TEST UNIT READY antes de dar la unidad por no lista. Un pendrive
/// contesta «unidad no lista, haciendose» mientras arranca.
const MSC_READY_ATTEMPTS: u8 = 8;
const MSC_READY_WAIT_US: u64 = 250_000;

/// Un hub configurado: lo que hay que recordar para enterarse de sus cambios de
/// puerto y para apuntar a su Transaction Translator desde los hijos.
struct HubDev {
    slot: u8,
    /// Código de velocidad del propio hub.
    speed: u8,
    ports: u8,
    multi_tt: bool,
    /// DCI del endpoint de cambio de estado, 0 si no se pudo armar (entonces el
    /// hub depende del sondeo de [`XhciInner::scan_hubs_if_due`]).
    ep_dci: u8,
    /// Marca de tiempo (µs) del último barrido de los puertos de este hub.
    scan_last_us: u64,
    /// Fuerza el próximo barrido sin esperar al periodo: lo pone un informe de
    /// cambio de estado que se perdió.
    scan_due: bool,
    /// Único búfer del endpoint, y el tamaño del mapa de bits que cabe en él.
    ///
    /// Uno basta, al contrario que en un HID: el hub reenvía el mismo mapa de
    /// bits hasta que se lee el estado del puerto que lo puso, así que no hay
    /// un flujo que seguir y perder una repetición no pierde información.
    buf: Option<DmaBuf>,
    change_len: usize,
}

/// Cuántas interfaces de un dispositivo se recuerdan. Un compuesto normal trae
/// dos o tres; ocho cubre un receptor inalámbrico con todo lo que lleva.
const MAX_IFACES_RECORDED: usize = 8;

/// Una interfaz tal como la declara el descriptor de configuración.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct IfaceRecord {
    num: u8,
    class: u8,
    subclass: u8,
    proto: u8,
}

/// Un dispositivo USB enumerado, con driver o sin él.
///
/// Antes de esto el driver solo se acordaba de lo que podía usar: una interfaz
/// HID en [`XhciInner::hids`], un hub en [`XhciInner::hubs`]. Todo lo demás se
/// direccionaba, se configuraba y se olvidaba, así que a la pregunta «¿el
/// sistema ve mi pendrive?» no se podía contestar ni que sí ni que no. Esta
/// lista es lo que la contesta, y es el punto de partida de cualquier driver de
/// clase que venga despues.
struct UsbDev {
    slot: u8,
    topo: DevTopo,
    speed: u8,
    vid: u16,
    pid: u16,
    /// `bDeviceClass` / `bDeviceSubClass` / `bDeviceProtocol`.
    class: u8,
    subclass: u8,
    proto: u8,
    ifaces: Vec<IfaceRecord>,
    /// Interfaces que el descriptor declaraba y no caben en `ifaces`.
    ifaces_dropped: u8,
}

/// El nombre de una clase USB, para que un volcado se lea sin la tabla delante.
///
/// Son los códigos que `usb.org` asigna y que de verdad aparecen en una
/// máquina; cualquier otro sale como su número en hexadecimal.
fn usb_class_name(class: u8) -> &'static str {
    match class {
        0x00 => "por interfaz",
        0x01 => "audio",
        0x02 => "cdc",
        USB_CLASS_HID => "hid",
        0x05 => "fisico",
        0x06 => "imagen",
        0x07 => "impresora",
        0x08 => "almacenamiento",
        USB_CLASS_HUB => "hub",
        0x0a => "datos-cdc",
        0x0b => "tarjeta-chip",
        0x0d => "seguridad",
        0x0e => "video",
        0x0f => "salud",
        0x10 => "audio-video",
        0x11 => "pantalla",
        0xdc => "diagnostico",
        0xe0 => "inalambrico",
        0xef => "varios",
        0xfe => "especifico",
        0xff => "del fabricante",
        _ => "?",
    }
}

/// Recorre un descriptor de configuración entregando cada descriptor suyo como
/// `(bDescriptorType, cuerpo)`, y para en cuanto uno no cabe o miente sobre su
/// longitud. Un `bLength` de 0 o 1 colgaría el recorrido.
fn config_descriptors(raw: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut o = 0usize;
    core::iter::from_fn(move || {
        if o + 2 > raw.len() {
            return None;
        }
        let dl = raw[o] as usize;
        if dl < 2 || o + dl > raw.len() {
            return None;
        }
        let d = &raw[o..o + dl];
        o += dl;
        Some((d[1], d))
    })
}

/// Las interfaces que declara un descriptor de configuración, en orden, y
/// cuántas se quedaron fuera por el tope de [`MAX_IFACES_RECORDED`].
///
/// Solo el ajuste alternativo 0 de cada una: `bAlternateSetting` distinto de
/// cero es otra cara de la MISMA interfaz, y contarlas por separado llena la
/// lista de duplicados y desplaza a las interfaces de verdad.
fn config_interfaces(raw: &[u8]) -> (Vec<IfaceRecord>, u8) {
    let mut out: Vec<IfaceRecord> = Vec::new();
    let mut dropped = 0u8;
    for (dt, d) in config_descriptors(raw) {
        if dt != USB_DESC_IFACE || d.len() < 9 || d[3] != 0 {
            continue;
        }
        if out.len() < MAX_IFACES_RECORDED {
            out.push(IfaceRecord {
                num: d[2],
                class: d[5],
                subclass: d[6],
                proto: d[7],
            });
        } else {
            dropped = dropped.saturating_add(1);
        }
    }
    (out, dropped)
}

/// El endpoint de interrupción IN de la primera interfaz de clase `class`:
/// `(bEndpointAddress, wMaxPacketSize, bInterval)`.
///
/// `wMaxPacketSize` viene enmascarado a sus bits 10:0: los 12:11 son el campo
/// de transacciones adicionales de alta velocidad y no pueden colarse en el
/// Max Packet Size del contexto del endpoint.
fn class_int_in_endpoint(raw: &[u8], class: u8) -> Option<(u8, u16, u8)> {
    let mut inside = false;
    for (dt, d) in config_descriptors(raw) {
        if dt == USB_DESC_IFACE && d.len() >= 9 {
            inside = d[5] == class;
        }
        if dt == USB_DESC_EP && d.len() >= 7 && inside {
            let addr = d[2];
            // Interrupción (bmAttributes bits 1:0 = 3) y dirección IN.
            if (addr & 0x80) != 0 && (d[3] & 3) == 3 {
                return Some((addr, u16::from_le_bytes([d[4], d[5]]) & 0x7ff, d[6]));
            }
        }
    }
    None
}

// ——— Almacenamiento masivo: Bulk-Only Transport y SCSI ———

const USB_CLASS_MASS_STORAGE: u8 = 0x08;
/// `bInterfaceProtocol` del Bulk-Only Transport, que es el que usa todo lo que
/// se vende hoy. CBI (0x00, 0x01) es de los noventa y no se cubre.
const MSC_PROTO_BULK_ONLY: u8 = 0x50;
/// `bInterfaceSubClass`: SCSI transparente. Los demas (RBC, MMC, UFI) hablan
/// otros conjuntos de ordenes.
const MSC_SUBCLASS_SCSI: u8 = 0x06;
/// `GET_MAX_LUN`, peticion de clase a la interfaz (USB MSC BOT §3.2).
const MSC_REQ_GET_MAX_LUN: u8 = 0xfe;

const BOT_CBW_LEN: usize = 31;
const BOT_CSW_LEN: usize = 13;
const BOT_CBW_SIGNATURE: u32 = 0x4342_5355; // "USBC"
const BOT_CSW_SIGNATURE: u32 = 0x5342_5355; // "USBS"

const SCSI_TEST_UNIT_READY: u8 = 0x00;
const SCSI_REQUEST_SENSE: u8 = 0x03;
const SCSI_INQUIRY: u8 = 0x12;
const SCSI_READ_CAPACITY_10: u8 = 0x25;
const SCSI_SERVICE_ACTION_IN_16: u8 = 0x9e;
const SCSI_SAI_READ_CAPACITY_16: u8 = 0x10;
/// Lo mas que cabe en el campo de longitud de un TRB Normal (xHCI §6.4.1.1).
///
/// El parametro de `trb_normal` es un `u16` y el campo son 17 bits, asi que
/// pedir 64 KiB justos daria longitud 0: una peticion que hay que partir, no
/// recortar. Con bloques de 512 son 127 bloques por vuelta.
const MSC_MAX_TRB_LEN: u32 = 65_535;
/// Tamano del bufer de rebote de cada unidad. Tiene que cubrir una vuelta
/// entera de `MSC_MAX_TRB_LEN`.
const MSC_BOUNCE_BYTES: usize = 64 * 1024;
/// Identidad que se le da a la siguiente unidad. Ver `MscDev::disk_id`.
static NEXT_DISK_ID: AtomicU64 = AtomicU64::new(1);

const SCSI_READ_10: u8 = 0x28;
const SCSI_WRITE_10: u8 = 0x2a;
const SCSI_SYNCHRONIZE_CACHE_10: u8 = 0x35;

/// READ(10) / WRITE(10) (SBC-3 §5.10 y §5.32).
///
/// Devuelve `None` en vez de recortar cuando la peticion no cabe en el CDB: el
/// LBA son cuatro bytes y la cuenta dos, asi que una peticion mas alla de
/// 2 TiB o de 65535 bloques no se puede expresar. Recortarla silenciosamente
/// escribiria en el sitio equivocado o dejaria media escritura hecha, y las
/// dos cosas corrompen el sistema de archivos; quien llama tiene que partirla.
fn scsi_rw10(op: u8, lba: u64, blocks: u32) -> Option<[u8; 10]> {
    if blocks == 0 || blocks > u16::MAX as u32 || lba > u32::MAX as u64 {
        return None;
    }
    let l = (lba as u32).to_be_bytes();
    let n = (blocks as u16).to_be_bytes();
    Some([op, 0, l[0], l[1], l[2], l[3], 0, n[0], n[1], 0])
}

/// SYNCHRONIZE CACHE(10) de toda la unidad (SBC-3 §5.26).
///
/// Cuenta de bloques 0 es «desde el LBA dado hasta el final», que es justo lo
/// que quiere un `flush()`, y es el unico sitio donde el cero no es un error.
fn scsi_sync_cache10() -> [u8; 10] {
    [SCSI_SYNCHRONIZE_CACHE_10, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// La fase de datos de una orden de disco, y en que sentido va.
///
/// Un solo enum en vez de dos metodos casi iguales, y sobre todo en vez de un
/// `&mut [u8]` para los dos sentidos: una escritura recibe un `&[u8]` del
/// sistema de archivos, y obligarla a copiarlo a un `Vec` mutable solo para
/// encajar en la firma anade una copia de cada sector escrito.
enum MscData<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

impl MscData<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Read(d) => d.len(),
            Self::Write(d) => d.len(),
        }
    }

    fn is_write(&self) -> bool {
        matches!(self, Self::Write(_))
    }
}

/// Un Command Block Wrapper (USB MSC BOT §5.1): los 31 bytes que abren cada
/// orden SCSI sobre el endpoint bulk OUT.
///
/// `data_len` es lo que se espera transferir en la fase de datos y `dir_in` su
/// direccion. El `tag` vuelve identico en el CSW y es lo unico que ata la
/// respuesta a su pregunta: creerse un CSW de otro tag es leer el resultado de
/// la orden anterior.
fn bot_cbw(
    tag: u32,
    data_len: u32,
    dir_in: bool,
    lun: u8,
    cmd: &[u8],
) -> Option<[u8; BOT_CBW_LEN]> {
    // bCBWCBLength son 5 bits y el bloque de orden son 16 bytes de hueco.
    if cmd.is_empty() || cmd.len() > 16 {
        return None;
    }
    let mut w = [0u8; BOT_CBW_LEN];
    w[0..4].copy_from_slice(&BOT_CBW_SIGNATURE.to_le_bytes());
    w[4..8].copy_from_slice(&tag.to_le_bytes());
    w[8..12].copy_from_slice(&data_len.to_le_bytes());
    // bmCBWFlags: solo el bit 7, la direccion. El resto es reservado-cero, y
    // un uno ahi es un CBW que el dispositivo puede rechazar entero.
    w[12] = if dir_in { 0x80 } else { 0x00 };
    w[13] = lun & 0x0f;
    w[14] = cmd.len() as u8;
    w[15..15 + cmd.len()].copy_from_slice(cmd);
    Some(w)
}

/// El resultado de una orden, tal como viene en el Command Status Wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BotCsw {
    tag: u32,
    /// Bytes de la fase de datos que NO se transfirieron.
    residue: u32,
    /// 0 = bien, 1 = la orden fallo (hay que pedir el sentido), 2 = error de
    /// fase (el dispositivo quiere un reset del transporte).
    status: u8,
}

/// Lee un CSW y comprueba que es el de `want_tag`.
///
/// `None` cuando no es un CSW valido o cuando contesta a otra orden: en los dos
/// casos la respuesta no se puede usar, y el transporte necesita recuperarse.
fn bot_parse_csw(raw: &[u8], want_tag: u32) -> Option<BotCsw> {
    if raw.len() < BOT_CSW_LEN {
        return None;
    }
    if u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) != BOT_CSW_SIGNATURE {
        return None;
    }
    let tag = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    if tag != want_tag {
        return None;
    }
    let status = raw[12];
    // 3 y 4 son reservados y 0x80 en adelante no existe: un dispositivo que
    // los manda no esta diciendo «bien».
    if status > 2 {
        return None;
    }
    Some(BotCsw {
        tag,
        residue: u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
        status,
    })
}

/// Lo que un INQUIRY cuenta del dispositivo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ScsiInquiry {
    /// `Peripheral Device Type`: 0 = disco de bloques, 5 = CD/DVD, 0x1f = nada
    /// conectado.
    dev_type: u8,
    removable: bool,
    vendor: [u8; 8],
    product: [u8; 16],
    revision: [u8; 4],
}

/// Parsea los 36 bytes de un INQUIRY estandar (SPC-4 §6.4).
fn scsi_parse_inquiry(raw: &[u8]) -> Option<ScsiInquiry> {
    if raw.len() < 36 {
        return None;
    }
    let mut out = ScsiInquiry {
        dev_type: raw[0] & 0x1f,
        // RMB es el bit 7 del byte 1; los otros siete son reservados.
        removable: raw[1] & 0x80 != 0,
        ..ScsiInquiry::default()
    };
    out.vendor.copy_from_slice(&raw[8..16]);
    out.product.copy_from_slice(&raw[16..32]);
    out.revision.copy_from_slice(&raw[32..36]);
    Some(out)
}

/// Un campo de texto de un INQUIRY, sin los espacios con que SCSI lo rellena y
/// sin los bytes que no se pueden imprimir.
///
/// SCSI rellena a la derecha con espacios, no con ceros, asi que volcarlo tal
/// cual deja una columna de huecos; y un dispositivo malo mete control ahi, que
/// en una linea de `/proc` rompe el formato de todo lo demas.
fn scsi_text(field: &[u8]) -> alloc::string::String {
    field
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect::<alloc::string::String>()
        .trim_end()
        .into()
}

/// La capacidad que devuelve un READ CAPACITY: `(ultimo LBA, bytes por bloque)`.
///
/// `read_capacity(10)` da el ultimo LBA en 32 bits, asi que `0xffff_ffff` no es
/// una capacidad sino «no cabe, preguntame con el de 16» (SBC-3 §5.15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScsiCapacity {
    last_lba: u64,
    block_size: u32,
    /// `true` cuando hay que repetir con READ CAPACITY(16).
    needs_16: bool,
}

fn scsi_parse_capacity10(raw: &[u8]) -> Option<ScsiCapacity> {
    if raw.len() < 8 {
        return None;
    }
    // SCSI es big-endian en todo.
    let last = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let bs = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
    if bs == 0 {
        return None;
    }
    Some(ScsiCapacity {
        last_lba: last as u64,
        block_size: bs,
        needs_16: last == u32::MAX,
    })
}

fn scsi_parse_capacity16(raw: &[u8]) -> Option<ScsiCapacity> {
    if raw.len() < 12 {
        return None;
    }
    let mut lba = [0u8; 8];
    lba.copy_from_slice(&raw[0..8]);
    let bs = u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]);
    if bs == 0 {
        return None;
    }
    Some(ScsiCapacity {
        last_lba: u64::from_be_bytes(lba),
        block_size: bs,
        needs_16: false,
    })
}

/// Sectores de 512 bytes que ocupa un disco de `last_lba` bloques de
/// `block_size`, que es la unidad con la que habla `BlockScheme`.
///
/// `last_lba` es el ULTIMO bloque, no la cuenta: un disco de un solo bloque
/// contesta 0, y tomarlo por la cuenta deja el ultimo sector fuera del disco.
fn scsi_sectors_512(cap: &ScsiCapacity) -> u64 {
    let blocks = cap.last_lba.saturating_add(1);
    if cap.block_size >= 512 {
        blocks.saturating_mul((cap.block_size / 512) as u64)
    } else {
        // Un bloque de menos de 512 bytes no existe en nada real, pero
        // multiplicar por cero dejaria un disco de capacidad 0 que parece
        // vacio en vez de parecer raro.
        blocks
    }
}

/// La pareja de endpoints bulk de la interfaz `iface`:
/// `(IN, OUT)`, cada uno `(bEndpointAddress, wMaxPacketSize)`.
///
/// `None` si a la interfaz le falta uno de los dos. Se buscan por numero de
/// interfaz y no «el primer bulk del descriptor» porque un disco externo con
/// lector de tarjetas declara dos interfaces de almacenamiento, y armar los
/// endpoints de una con el numero de la otra es hablarle al disco equivocado.
///
/// Solo el ajuste alternativo 0, como [`config_interfaces`]: los endpoints que
/// siguen a una alternativa pertenecen a esa alternativa, que no es la que se
/// ha seleccionado.
fn iface_bulk_endpoints(raw: &[u8], iface: u8) -> Option<((u8, u16), (u8, u16))> {
    let mut inside = false;
    let mut ep_in = None;
    let mut ep_out = None;
    for (dt, d) in config_descriptors(raw) {
        if dt == USB_DESC_IFACE && d.len() >= 9 {
            inside = d[2] == iface && d[3] == 0;
            continue;
        }
        if dt != USB_DESC_EP || d.len() < 7 || !inside {
            continue;
        }
        // bmAttributes bits 1:0 = 2 es bulk.
        if d[3] & 3 != 2 {
            continue;
        }
        let mps = u16::from_le_bytes([d[4], d[5]]) & 0x7ff;
        if mps == 0 {
            continue;
        }
        let slot = if d[2] & 0x80 != 0 {
            &mut ep_in
        } else {
            &mut ep_out
        };
        // El primero de cada direccion: una interfaz con mas de una pareja
        // tiene la suya en la primera.
        if slot.is_none() {
            *slot = Some((d[2], mps));
        }
    }
    Some((ep_in?, ep_out?))
}

/// Lo que de un hub se lee sin tocar su lista: `(velocidad, puertos, multi_tt)`.
fn hub_facts(h: &HubDev) -> (u8, u8, bool) {
    (h.speed, h.ports, h.multi_tt)
}

struct HidDev {
    slot_id: u8,
    port_id: u8,
    ep_dci: u8,
    ring_idx: usize,
    protocol: u8,
    report_len: usize,
    /// VirtualBox USB Tablet (`80ee:0021`) puts X/Y *after* wheel/hwheel/pad:
    /// `[buttons, dz, dw, pad, X16, Y16]`. QEMU usb-tablet is
    /// `[buttons, X16, Y16, wheel]`. Parsing VBox packets as QEMU made the
    /// guest cursor ignore host mouse integration.
    vbox_tablet: bool,
    /// Round-robin ring of report buffers, one per pre-queued TRB on this
    /// endpoint (see [`HID_QUEUE_DEPTH`]). The controller fills them in
    /// enqueue order; we drain in the same order.
    bufs: Vec<DmaBuf>,
    /// Index into `bufs` of the next buffer whose transfer event will be
    /// dispatched. Advances after every consumed completion.
    dispatch_idx: usize,
    /// Index into `bufs` of the next buffer to be re-armed with a fresh TRB.
    /// Advances after every resubmit.
    enqueue_idx: usize,
    /// Keyboard modifier bitmap of the last report.
    last_mods: u8,
    /// Mouse button bitmap of the last report. Separate from `last_mods`: one
    /// interface can carry both roles, and sharing the field made every
    /// keystroke on a combo receiver look like a button change (and every
    /// click like a modifier change).
    last_buttons: u8,
    last_keys: [u8; 6],
    /// Diagnostics for `/proc/usbhid` (see `XhciUsbHid::debug_report`).
    iface: u8,
    if_proto: u8,
    subclass: u8,
    vid: u16,
    pid: u16,
    /// Bytes of the most recently dispatched report, and its length.
    last_report: [u8; 16],
    last_report_len: usize,
    /// Count of reports seen on this endpoint (0 = never delivered a report).
    report_count: u64,
    /// Count of reports that carried a non-zero wheel or pan delta, and the
    /// last such pair. Together with `report_count` this splits the one
    /// question a dead wheel always poses -- does the driver never decode a
    /// detent, or does it decode them and something above drops them? -- into
    /// an answer anyone can read off `/proc/usbhid` after scrolling.
    wheel_count: u64,
    last_wheel: (i32, i32),
    /// First bytes of the HID report descriptor (proto-0 interfaces), for
    /// /proc/usbhid. Empty for boot-protocol devices (no descriptor read).
    report_desc: [u8; REPORT_DESC_SNAPSHOT],
    report_desc_len: usize,
    /// Parsed relative-mouse report layout, when the descriptor gave one.
    /// `None` → parse the boot `[buttons, dx, dy, …]` layout.
    mouse_layout: MouseReports,
    /// Latched when this interface turned out to be speaking boot protocol
    /// after all (it refused SET_PROTOCOL(Report), or its BIOS left it there):
    /// its reports are shorter than `mouse_layout` describes, so decode the
    /// fixed boot layout instead of reading zeros off the end of every report.
    boot_reports: bool,
    /// Parsed keyboard report layout. [`BOOT_KEY_LAYOUT`] for a boot-protocol
    /// keyboard (no report descriptor is read for those).
    key_layout: Option<KeyLayout>,
}

/// `bEndpointAddress` of the endpoint a Device Context Index names.
/// DCI = 2*N for an OUT endpoint, 2*N+1 for an IN one; DCI 1 is EP0.
fn ep_addr_from_dci(dci: u8) -> u16 {
    let ep_num = (dci >> 1) as u16;
    if dci & 1 == 1 {
        ep_num | 0x80
    } else {
        ep_num
    }
}

/// Does this Transfer Event belong to the control transfer whose three TRBs
/// sit at `setup` / `data` / `status`?
///
/// VirtualBox sometimes reports `ev.p` as the TRB AFTER the completed one, so
/// each address is accepted at itself and one slot past it. A zero address
/// means that stage is absent (a control transfer with no data stage) and
/// matches nothing.
fn ep0_event_belongs(setup: u64, data: u64, status: u64, p: u64) -> bool {
    [setup, data, status]
        .iter()
        .any(|&t| t != 0 && (p == t || p == t.wrapping_add(16)))
}

impl XhciInner {
    fn new(mmio: XhciMmio, max_slots: u8, max_ports: u8, msi_vector: usize) -> DeviceResult<Self> {
        let hcc = mmio.read_cap(0x10);
        let context_size = if ((hcc >> 2) & 1) != 0 { 64 } else { 32 };
        let ns = max_slots as usize + 1;
        let dcbaa = DmaBuf::new(ns * 8, 4096)?;
        let mut dev_ctx = Vec::with_capacity(ns);
        dev_ctx.resize_with(ns, || None);
        let nr = ns * 32;
        let mut xfer_rings = Vec::with_capacity(nr);
        xfer_rings.resize_with(nr, || None);
        Ok(Self {
            mmio,
            cmd: CmdRing::new(64)?,
            ev: EventRing::new(256)?,
            dcbaa,
            max_slots,
            max_ports,
            context_size,
            msi_vector,
            slot_speed: alloc::vec![0; ns],
            slot_topo: alloc::vec![None; ns],
            dev_ctx,
            xfer_rings,
            scratch_tbl: None,
            scratch_pages: Vec::new(),
            hids: Vec::new(),
            pending_port_changes: Vec::new(),
            port_enum_fails: alloc::vec![0u8; max_ports as usize + 2],
            pending_ep_resets: Vec::new(),
            devs: Vec::new(),
            mscs: Vec::new(),
            pending_disk_regs: Vec::new(),
            pending_disk_unregs: Vec::new(),
            hubs: Vec::new(),
            pending_hub_ports: Vec::new(),
            boot_enum_pending: true,
            halt_attempts: 0,
            halt_last_attempt_us: 0,
        })
    }

    fn ri(slot: u8, dci: u8) -> usize {
        slot as usize * 32 + dci as usize
    }

    fn pop_ev(&mut self, lis: Option<&EventListener<InputEvent>>) -> Option<Trb> {
        if let Some(trb) = self.ev.pop() {
            let etype = (trb.ctrl >> 10) & 0x3f;
            let erdp = self.ev.erdp_phys();
            self.mmio.write_rt64(0x38, (erdp & !0xf) | 0x8);

            if etype == 32 {
                // TRB_EVT_TRANSFER. Always run the transfer-side handler, even
                // during enumeration (lis=None from wait_ep0*): it re-arms the
                // completed HID interrupt endpoint (advance dequeue + resubmit)
                // and only *dispatches* a report when `lis` is Some. Skipping it
                // for lis=None (the old behaviour) left an already-enumerated
                // keyboard/mouse endpoint's ring un-advanced whenever a report
                // landed mid-enumeration of another device — a permanent 1-slot
                // desync per drop. Control (EP0, dci=1) transfer events match no
                // HID endpoint, so the handler is a no-op for them and the EP0
                // waiter still receives the TRB.
                self.handle_hid_transfer_side(&trb, lis);
            } else if etype == 34 {
                // TRB_EVT_PORT_STATUS: diferir para evitar re-entrada recursiva
                // durante la enumeración (pop_ev -> try_port_hid -> pop_ev).
                let port_id = ((trb.p >> 24) & 0xff) as u8;
                if port_id >= 1 && !self.pending_port_changes.contains(&port_id) {
                    self.pending_port_changes.push(port_id);
                }
            }

            return Some(trb);
        }
        None
    }

    /// Procesar cambios de puerto diferidos. Llamar solo desde contextos no-reentrantes
    /// (process_irq_events, poll, enumerate_root_hid tras cada puerto).
    fn drain_pending_port_changes(&mut self) {
        let ports: Vec<u8> = core::mem::take(&mut self.pending_port_changes);
        for port_id in ports {
            let _ = self.handle_port_status_change(port_id);
        }
    }

    fn resubmit_hid_normal_trb(
        &mut self,
        ring_idx: usize,
        buf_phys: u64,
        len: u16,
        slot: u8,
        ep: u8,
    ) {
        let trb = trb_normal(buf_phys, len, true);
        if let Some(r) = self.xfer_rings.get_mut(ring_idx).and_then(|o| o.as_mut()) {
            match r.push(trb) {
                Ok(_) => {
                    fence(Ordering::SeqCst);
                    self.mmio.ring_db(slot, ep);
                }
                Err(_) => {
                    r.advance_dequeue(1);
                    if r.push(trb_normal(buf_phys, len, true)).is_ok() {
                        fence(Ordering::SeqCst);
                        self.mmio.ring_db(slot, ep);
                    }
                }
            }
        }
    }

    fn handle_hid_transfer_side(
        &mut self,
        ev: &Trb,
        lis: Option<&EventListener<InputEvent>>,
    ) -> bool {
        let ty = (ev.ctrl >> 10) & 0x3f;
        if ty != 32 {
            // TRB_EVT_TRANSFER
            return false;
        }
        let i = ((ev.ctrl >> 24) & 0xff) as usize; // slot
        let dci = ((ev.ctrl >> 16) & 0x1f) as u8;
        let cc = (ev.status >> 24) & 0xff;

        if i == 0 || i > self.max_slots as usize {
            return false;
        }

        // El endpoint de cambio de estado de un hub no lleva informes HID: su
        // carga es un mapa de bits de puertos, y su anillo tiene un solo TRB.
        if self
            .hubs
            .iter()
            .any(|h| h.slot == i as u8 && h.ep_dci == dci && dci != 0)
        {
            return self.handle_hub_status_event(i as u8, ev, cc);
        }

        if cc != TRB_CC_SUCCESS && cc != TRB_CC_SHORT {
            // A failed transfer. Skip its TRB, re-arm one in its place, and --
            // only when the completion code actually leaves the endpoint
            // Halted -- queue a deferred Reset Endpoint (commands must not run
            // nested inside this event drain).
            if let Some(idx) = self
                .hids
                .iter()
                .position(|h| h.slot_id == i as u8 && h.ep_dci == dci)
            {
                let (ridx, blen, buf_phys) = {
                    let h = &mut self.hids[idx];
                    // No report was dispatched for this transfer, so the
                    // dispatch head moves with the enqueue head below and the
                    // two stay in lockstep.
                    h.dispatch_idx = (h.dispatch_idx + 1) % HID_QUEUE_DEPTH;
                    (
                        h.ring_idx,
                        h.report_len as u16,
                        h.bufs[h.enqueue_idx].sub_phys(0),
                    )
                };
                if let Some(r) = self.xfer_rings.get_mut(ridx).and_then(|o| o.as_mut()) {
                    r.advance_dequeue(1);
                }
                // Re-arm HERE, once per failed transfer, rather than once per
                // deferred reset. The deferred list is deduplicated by
                // (slot, dci), so a burst of N failures on one endpoint used to
                // put back a single TRB: the ring lost N-1 of its
                // HID_QUEUE_DEPTH slots every burst and the two heads drifted
                // apart by N-1 for good -- reports then came out of the wrong
                // buffer, and after a few bursts the ring ran empty and the
                // device went silent until it was unplugged. Pushing onto a
                // halted endpoint is harmless: the TRB simply waits there, and
                // `deq_phys()` below is unaffected because the dequeue head
                // does not move on a push.
                self.resubmit_hid_normal_trb(ridx, buf_phys, blen, i as u8, dci);
                if let Some(h) = self.hids.get_mut(idx) {
                    h.enqueue_idx = (h.enqueue_idx + 1) % HID_QUEUE_DEPTH;
                }
                if cc_halts_endpoint(cc) {
                    // CC 6 is Stall Error (CC 5 is TRB Error).
                    let stalled = cc == 6;
                    if let Some(e) = self
                        .pending_ep_resets
                        .iter_mut()
                        .find(|(s, d, _)| *s == i as u8 && *d == dci)
                    {
                        e.2 |= stalled;
                    } else {
                        self.pending_ep_resets.push((i as u8, dci, stalled));
                    }
                } else {
                    // Missed Service Error is the one a real xHCI actually
                    // produces on a busy bus, and it does NOT halt anything:
                    // the host skipped one service interval. Resetting a
                    // RUNNING endpoint answers Context State Error, so the old
                    // unconditional reset turned a dropped report into a failed
                    // recovery -- which is also why this never showed up under
                    // QEMU, where the code is never emitted.
                    warn!(
                        "[xhci] slot={} dci={} transfer cc={} (endpoint not halted); TRB skipped",
                        i, dci, cc
                    );
                }
            }
            return false;
        }

        let hid_i = self
            .hids
            .iter()
            .position(|h| h.slot_id == i as u8 && h.ep_dci == dci);

        let idx = match hid_i {
            Some(idx) => idx,
            None => return false,
        };

        // Resynchronise the dispatch head from the event itself. `ev.p` is the
        // TRB the controller completed, and that TRB carries the address of
        // the buffer it filled: comparing it against `bufs` says which report
        // just landed, with no dependence on a counter. A single dropped or
        // reordered event used to put the software head permanently one slot
        // behind, and from then on every report was decoded out of the
        // PREVIOUS completion's buffer -- the pointer lagging one motion, keys
        // arriving on release -- with nothing to ever bring it back.
        {
            let (ring_idx, want) = {
                let h = &self.hids[idx];
                (h.ring_idx, h.bufs[h.dispatch_idx].sub_phys(0))
            };
            let seen = self
                .xfer_rings
                .get(ring_idx)
                .and_then(|o| o.as_ref())
                .and_then(|r| {
                    // VirtualBox sometimes reports `ev.p` as the TRB AFTER the
                    // completed one. If the previous TRB is the buffer we
                    // expect, believe the counter and leave it alone.
                    if r.trb_buffer_at(r.prev_trb_phys(ev.p)) == Some(want) {
                        None
                    } else {
                        r.trb_buffer_at(ev.p)
                    }
                });
            if let Some(filled) = seen {
                if filled != want {
                    if let Some(k) = self.hids[idx]
                        .bufs
                        .iter()
                        .position(|b| b.sub_phys(0) == filled)
                    {
                        warn!(
                            "[xhci] slot={} dci={} dispatch head {} -> {} (resynced from the event)",
                            i, dci, self.hids[idx].dispatch_idx, k
                        );
                        self.hids[idx].dispatch_idx = k;
                    }
                }
            }
        }

        // Bytes actually written by the controller. The Transfer Event carries
        // the RESIDUAL in its low 24 bits (xHCI 1.2 section 6.4.2.1), i.e. what
        // was NOT transferred; a short report used to be decoded over the full
        // buffer, so the tail of the PREVIOUS report in that same buffer was
        // read as live data -- stuck buttons and phantom keys on any device
        // whose reports vary in length.
        let residual = (ev.status & 0x00ff_ffff) as usize;

        let (ridx, blen, buf_phys, dispatch_idx) = {
            let h = &mut self.hids[idx];
            // Re-arm with the buffer at the enqueue head of the round-robin
            // ring (its TRB is the one we're about to push back into the
            // ring). dispatch_hid drains from a separate head so reads see
            // the report whose event we're processing, not the one the
            // controller might be writing next.
            //
            // Advance the dispatch head HERE, unconditionally — not inside
            // dispatch_hid. Completions consumed with `lis == None` (from the
            // EP0/command waiters during another device's enumeration or an
            // endpoint recovery) used to advance only `enqueue_idx`, leaving
            // this endpoint's dispatch head one slot behind for good: every
            // later report was read from the PREVIOUS completion's buffer
            // (key presses showing up on release, clicks one motion late).
            let buf_phys = h.bufs[h.enqueue_idx].sub_phys(0);
            let dispatch_idx = h.dispatch_idx;
            h.dispatch_idx = (dispatch_idx + 1) % HID_QUEUE_DEPTH;
            (h.ring_idx, h.report_len as u16, buf_phys, dispatch_idx)
        };
        if let Some(l) = lis {
            let actual = (blen as usize).saturating_sub(residual);
            self.dispatch_hid(idx, dispatch_idx, actual, l);
        }
        if let Some(r) = self.xfer_rings.get_mut(ridx).and_then(|o| o.as_mut()) {
            r.advance_dequeue(1);
        }
        self.resubmit_hid_normal_trb(ridx, buf_phys, blen, i as u8, dci);
        if let Some(h) = self.hids.get_mut(idx) {
            h.enqueue_idx = (h.enqueue_idx + 1) % HID_QUEUE_DEPTH;
        }
        true
    }

    fn wait_cmd_phys(&mut self, cmd_trb_phys: u64) -> DeviceResult<()> {
        let timeout_us = 5_000_000;
        let start = timer_now_us();
        let mut spins = 0u64;
        while !xhci_wait_expired(start, timeout_us, spins) {
            // 5s
            if let Some(ev) = self.pop_ev(None) {
                let ty = (ev.ctrl >> 10) & 0x3f;
                if ty == 33 {
                    // TRB_EVT_CMD_COMP
                    let match_addr = ev.p == cmd_trb_phys;
                    if match_addr {
                        let cc = (ev.status >> 24) & 0xff;
                        if cc == TRB_CC_SUCCESS || cc == TRB_CC_SHORT {
                            return Ok(());
                        }
                        warn!("[xhci] comando falló con CC={}", cc);
                        return Err(DeviceError::IoError);
                    }
                }
            }
            spins = spins.saturating_add(1);
            spin_loop();
        }
        if spins >= xhci_wait_spin_limit(timeout_us) {
            warn!("[xhci] wait_cmd_phys salió por guard de spins (timer estancado?)");
        }
        error!("[xhci] timeout esperando comando");
        Err(DeviceError::IoError)
    }

    fn wait_cmd_phys_slot(&mut self, cmd_trb_phys: u64) -> DeviceResult<u8> {
        let timeout_us = 5_000_000;
        let start = timer_now_us();
        let mut spins = 0u64;
        while !xhci_wait_expired(start, timeout_us, spins) {
            // 5s
            if let Some(ev) = self.pop_ev(None) {
                let ty = (ev.ctrl >> 10) & 0x3f;
                if ty == 33 {
                    // TRB_EVT_CMD_COMP
                    let match_addr = ev.p == cmd_trb_phys;
                    let slot_id = (ev.ctrl >> 24) & 0xff;
                    if match_addr {
                        let cc = (ev.status >> 24) & 0xff;
                        if cc != TRB_CC_SUCCESS && cc != TRB_CC_SHORT {
                            warn!("[xhci] comando de slot falló con CC={}", cc);
                            return Err(DeviceError::IoError);
                        }
                        return Ok(slot_id as u8);
                    }
                }
            }
            spins = spins.saturating_add(1);
            spin_loop();
        }
        if spins >= xhci_wait_spin_limit(timeout_us) {
            warn!("[xhci] wait_cmd_phys_slot salió por guard de spins (timer estancado?)");
        }
        error!("[xhci] timeout esperando comando de slot");
        Err(DeviceError::IoError)
    }

    fn exec_cmd(&mut self, t: Trb) -> DeviceResult<()> {
        let p = self.cmd.push(t)?;
        self.mmio.ring_db(0, 0);
        self.wait_cmd_phys(p)
    }

    /// Clear the device's own ENDPOINT_HALT feature (USB 2.0 section 9.4.1).
    ///
    /// Reset Endpoint and Set TR Dequeue Pointer un-halt the HOST side only.
    /// The device keeps its halt feature set and its data toggle where it was,
    /// so it STALLs the very next transaction and we reset again: an endless
    /// error -> reset -> error loop with no reports coming out, which is what a
    /// stalled mouse looks like from userspace. `usb_clear_halt()` in Linux
    /// does exactly this control request and then resets the host side.
    fn clear_endpoint_halt(&mut self, slot: u8, dci: u8) -> DeviceResult<()> {
        let addr = ep_addr_from_dci(dci);
        self.ep0_control_out0_optional(
            slot,
            // bmRequestType 0x02 (host->device, standard, endpoint),
            // bRequest 1 (CLEAR_FEATURE), wValue 0 (ENDPOINT_HALT).
            trb_setup(0x02, 0x01, 0x0000, addr, 0, 0),
            true,
        )
    }

    fn reset_endpoint_and_dequeue(&mut self, slot: u8, dci: u8) -> DeviceResult<()> {
        info!(
            "[xhci] reset_endpoint_and_dequeue slot={} dci={}",
            slot, dci
        );
        let p_reset = self.cmd.push(trb_reset_endpoint(slot, dci))?;
        self.mmio.ring_db(0, 0);
        if let Err(e) = self.wait_cmd_phys(p_reset) {
            warn!("[xhci] reset endpoint command failed: {:?}", e);
        }

        let ri = Self::ri(slot, dci);
        if let Some(ring) = self.xfer_rings.get(ri).and_then(|o| o.as_ref()) {
            let deq_phys = ring.deq_phys();
            let dcs = ring.deq_cycle();
            info!(
                "[xhci] set tr dequeue pointer: deq_phys={:#x} dcs={}",
                deq_phys, dcs
            );
            let p_deq = self
                .cmd
                .push(trb_set_tr_dequeue_pointer(deq_phys, dcs, slot, dci))?;
            self.mmio.ring_db(0, 0);
            if let Err(e) = self.wait_cmd_phys(p_deq) {
                warn!("[xhci] set tr dequeue pointer command failed: {:?}", e);
            }
        }
        Ok(())
    }

    fn wait_ep0_status_any(
        &mut self,
        slot: u8,
        setup_phys: u64,
        data_phys: u64,
        status_phys: u64,
        n_trb: usize,
        timeout_us: u64,
    ) -> DeviceResult<()> {
        let speed = self.slot_speed[slot as usize];
        info!(
            "[xhci] EP0 slot={} speed={} esperando: setup={:#x} data={:#x} status={:#x}",
            slot, speed, setup_phys, data_phys, status_phys
        );
        // Which event belongs to THIS control transfer. VirtualBox sometimes
        // reports `ev.p` as the TRB after the completed one, so each of our
        // three TRBs is accepted at its own address and one slot past it.
        //
        // The old test was "anywhere in [min, max+16)", a window that spans
        // the whole EP0 ring the moment a transfer straddles the ring wrap
        // (status_phys below setup_phys) -- and then the answer to some
        // OTHER request could satisfy this wait, which is how a device ends up
        // configured from a descriptor it never sent.

        let start = timer_now_us();
        let mut spins = 0u64;
        let mut ev_count = 0u32;
        while !xhci_wait_expired(start, timeout_us, spins) {
            if let Some(ev) = self.pop_ev(None) {
                let ty = (ev.ctrl >> 10) & 0x3f;
                ev_count += 1;
                if ty == 32 {
                    let ev_slot = ((ev.ctrl >> 24) & 0xff) as u8;
                    let ev_dci = ((ev.ctrl >> 16) & 0x1f) as u8;
                    let cc = (ev.status >> 24) & 0xff;
                    info!(
                        "[xhci] EP0 ev#{}: type=Transfer slot={} dci={} p={:#x} CC={}",
                        ev_count, ev_slot, ev_dci, ev.p, cc
                    );
                    // Matching flexible: dirección exacta (QEMU) o dentro del rango de la
                    // transferencia de control EP0 (VirtualBox puede reportar TRB+N).
                    if ev_slot == slot
                        && ep0_event_belongs(setup_phys, data_phys, status_phys, ev.p)
                    {
                        let i = Self::ri(slot, 1);
                        if let Some(r) = self.xfer_rings.get_mut(i).and_then(|o| o.as_mut()) {
                            r.advance_dequeue(n_trb);
                        }
                        // CC=1 Success, CC=13 Short Packet: ambos son éxito para Control.
                        // CC=6 (TRB Error) es también success en Status Stage de algunos HCs
                        // (VirtualBox, algunos Intel xHCI físicos).
                        if cc == TRB_CC_SUCCESS || cc == TRB_CC_SHORT {
                            info!("[xhci] EP0 slot={} OK (CC={})", slot, cc);
                            return Ok(());
                        }
                        if cc == 6 {
                            // Stall Error: the device refused the request.
                            // Callers treat a stalled optional request as
                            // "unsupported", which is what a STALL means.
                            info!(
                                "[xhci] EP0 slot={} STALL (CC=6). Clearing halt, returning OK.",
                                slot
                            );
                            let _ = self.clear_endpoint_halt(slot, 1);
                            let _ = self.reset_endpoint_and_dequeue(slot, 1);
                            return Ok(());
                        }
                        warn!(
                            "[xhci] EP0 slot={} error CC={}. Intentando reset_endpoint.",
                            slot, cc
                        );
                        let _ = self.reset_endpoint_and_dequeue(slot, 1);
                        return Err(DeviceError::IoError);
                    } else {
                        // Evento descartado — diagnosticar por qué
                        if ev_slot != slot {
                            // evento de otro slot, normal durante enumeración concurrente
                        } else {
                            warn!("[xhci] EP0 slot={} ev p={:#x} FUERA de rango", slot, ev.p);
                        }
                    }
                } else {
                    info!("[xhci] EP0 espera: ev#{} type={}", ev_count, ty);
                }
            }
            spins = spins.saturating_add(1);
            spin_loop();
        }
        if spins >= xhci_wait_spin_limit(timeout_us) {
            warn!(
                "[xhci] EP0 slot={} salió por guard de spins (timer estancado?)",
                slot
            );
        }
        error!(
            "[xhci] timeout EP0 slot={} ({} eventos vistos, setup={:#x})",
            slot, ev_count, setup_phys
        );
        let sts = self.mmio.read_op(4);
        error!("[xhci] USBSTS={:#010x}", sts);
        // Step the software dequeue past the TDs that never completed. Without
        // this, `reset_endpoint_and_dequeue` points the controller's TR
        // Dequeue Pointer back at the Setup TRB of the request that just timed
        // out, so the next doorbell replays it -- and its late answer then
        // satisfies the wait belonging to whatever request came after.
        let ri = Self::ri(slot, 1);
        if let Some(r) = self.xfer_rings.get_mut(ri).and_then(|o| o.as_mut()) {
            r.advance_dequeue(n_trb);
        }
        let _ = self.reset_endpoint_and_dequeue(slot, 1);
        Err(DeviceError::IoError)
    }

    fn ep0_control_in(&mut self, slot: u8, setup: Trb, buf: &DmaBuf, len: u32) -> DeviceResult<()> {
        let i = Self::ri(slot, 1);
        // Diagnóstico: estado del anillo EP0 antes de pushear TRBs
        if let Some(r) = self.xfer_rings.get(i).and_then(|o| o.as_ref()) {
            warn!(
                "[xhci] ep0_ctrl_in slot={} ring.phys={:#x} enq={} cycle={} cap={}",
                slot, r.buf.phys, r.enq, r.cycle as u8, r.cap
            );
        }
        let (setup_phys, data_phys, status_phys) = {
            let ring = self
                .xfer_rings
                .get_mut(i)
                .and_then(|o| o.as_mut())
                .ok_or(DeviceError::NotSupported)?;
            let st = trb_data(buf.sub_phys(0), len, true); // Data Stage is IN (true)
            let su = trb_status(false, true); // Status Stage is OUT (false)
            let p1 = ring.push(setup)?;
            let p2 = ring.push(st)?;
            let p3 = ring.push(su)?;
            (p1, p2, p3)
        };
        // Flush the transfer ring so the controller sees the TRBs before we ring the doorbell.
        if let Some(r) = self.xfer_rings.get(i).and_then(|o| o.as_ref()) {
            r.buf.flush(0, r.buf.len);
        }
        self.mmio.ring_db(slot, 1);
        self.wait_ep0_status_any(slot, setup_phys, data_phys, status_phys, 3, 2_000_000)
    }

    fn ep0_control_out0(&mut self, slot: u8, setup: Trb, status_in: bool) -> DeviceResult<()> {
        let i = Self::ri(slot, 1);
        let (setup_phys, status_phys) = {
            let ring = self
                .xfer_rings
                .get_mut(i)
                .and_then(|o| o.as_mut())
                .ok_or(DeviceError::NotSupported)?;
            let su = trb_status(status_in, true);
            let p1 = ring.push(setup)?;
            let p2 = ring.push(su)?;
            (p1, p2)
        };
        if let Some(r) = self.xfer_rings.get(i).and_then(|o| o.as_ref()) {
            r.buf.flush(0, r.buf.len);
        }
        self.mmio.ring_db(slot, 1);
        self.wait_ep0_status_any(slot, setup_phys, setup_phys, status_phys, 2, 2_000_000)
    }

    /// Igual que ep0_control_out0 pero con timeout corto (200ms) para comandos opcionales
    /// HID como SET_IDLE o SET_PROTOCOL que algunos dispositivos no responden.
    fn ep0_control_out0_optional(
        &mut self,
        slot: u8,
        setup: Trb,
        status_in: bool,
    ) -> DeviceResult<()> {
        let i = Self::ri(slot, 1);
        let (setup_phys, status_phys) = {
            let ring = self
                .xfer_rings
                .get_mut(i)
                .and_then(|o| o.as_mut())
                .ok_or(DeviceError::NotSupported)?;
            let su = trb_status(status_in, true);
            let p1 = ring.push(setup)?;
            let p2 = ring.push(su)?;
            (p1, p2)
        };
        if let Some(r) = self.xfer_rings.get(i).and_then(|o| o.as_ref()) {
            r.buf.flush(0, r.buf.len);
        }
        self.mmio.ring_db(slot, 1);
        // 200ms: suficiente para dispositivos lentos, no bloquea si el dispositivo
        // no responde (SET_IDLE es opcional en USB HID spec).
        self.wait_ep0_status_any(slot, setup_phys, setup_phys, status_phys, 2, 200_000)
    }

    pub fn reset_and_run(&mut self) -> DeviceResult<()> {
        let m = &self.mmio;
        m.perform_bios_handoff();

        // 1. Detener el controlador si está corriendo (spec §4.2.1)
        let usbcmd = m.read_op(0);
        if (usbcmd & 1) != 0 {
            info!("[xhci] deteniendo controlador antes del reset");
            m.write_op(0, usbcmd & !1); // RS=0
            let timeout_us = 100_000;
            let start = timer_now_us();
            let mut spins = 0u64;
            while (m.read_op(4) & 1) == 0 && !xhci_wait_expired(start, timeout_us, spins) {
                spins = spins.saturating_add(1);
                spin_loop();
            }
            if spins >= xhci_wait_spin_limit(timeout_us) {
                warn!("[xhci] stop-before-reset salió por guard de spins (timer estancado?)");
            }
        }

        // 2. Esperar a que CNR (Controller Not Ready) se limpie antes del reset
        let timeout_us = 1_000_000;
        let start = timer_now_us();
        let mut spins = 0u64;
        while (m.read_op(4) & (1 << 11)) != 0 && !xhci_wait_expired(start, timeout_us, spins) {
            spins = spins.saturating_add(1);
            spin_loop();
        }
        if spins >= xhci_wait_spin_limit(timeout_us) {
            warn!("[xhci] wait-CNR-before-reset salió por guard de spins (timer estancado?)");
        }
        if (m.read_op(4) & (1 << 11)) != 0 {
            error!("[xhci] timeout esperando CNR antes de reset");
            return Err(DeviceError::NotReady);
        }

        // 3. Emitir HCRST (bit 1 de USBCMD)
        info!("[xhci] emitiendo HCRST");
        m.write_op(0, m.read_op(0) | 2);
        let timeout_us = 100_000;
        let start = timer_now_us();
        let mut spins = 0u64;
        while (m.read_op(0) & 2) != 0 && !xhci_wait_expired(start, timeout_us, spins) {
            spins = spins.saturating_add(1);
            spin_loop();
        }
        if spins >= xhci_wait_spin_limit(timeout_us) {
            warn!("[xhci] wait-HCRST-clear salió por guard de spins (timer estancado?)");
        }

        // 4. Esperar a que CNR se limpie de nuevo tras reset
        let timeout_us = 1_000_000;
        let start = timer_now_us();
        let mut spins = 0u64;
        while (m.read_op(4) & (1 << 11)) != 0 && !xhci_wait_expired(start, timeout_us, spins) {
            spins = spins.saturating_add(1);
            spin_loop();
        }
        if spins >= xhci_wait_spin_limit(timeout_us) {
            warn!("[xhci] wait-CNR-after-reset salió por guard de spins (timer estancado?)");
        }
        if (m.read_op(4) & (1 << 11)) != 0 {
            error!("[xhci] timeout esperando CNR tras reset");
            return Err(DeviceError::NotReady);
        }
        info!("[xhci] reset completado");

        // Configurar Scratchpad buffers si son necesarios
        let hcsp2 = m.read_cap(8);
        let sb_lo = (hcsp2 >> 27) & 0x1f;
        let sb_hi = (hcsp2 >> 21) & 0x1f;
        let sb = sb_lo | (sb_hi << 5);
        if sb > 0 {
            info!("[xhci] reservando {} scratchpad buffers", sb);
            let tbl = DmaBuf::new(sb as usize * 8, 64)?;
            for i in 0..sb as usize {
                let pg = DmaBuf::new(PAGE_SIZE, PAGE_SIZE)?;
                tbl.write_u64(i * 8, pg.sub_phys(0));
                // The page was CPU-zeroed by DmaBuf::new and the controller
                // DMAs into it from the moment RS=1; evict those dirty lines
                // now, or a later writeback lands on controller-owned state.
                pg.flush(0, PAGE_SIZE);
                self.scratch_pages.push(pg);
            }
            tbl.flush(0, sb as usize * 8);
            self.dcbaa.write_u64(0, tbl.sub_phys(0));
            // Every per-slot DCBAA write is flushed; entry 0 -- the scratchpad
            // array pointer, and the first thing the controller reads once
            // DCBAAP is programmed below -- was the one that was not.
            self.dcbaa.flush(0, 8);
            self.scratch_tbl = Some(tbl);
        }

        // Configurar Max Slots y bases de datos
        let cfg = m.read_op(0x38);
        m.write_op(0x38, (cfg & !0xff) | self.max_slots as u32);

        m.write_op64(0x30, self.dcbaa.phys as u64);

        let crcr = self.cmd.crcr();
        m.write_op64(0x18, (crcr & !0x3F) | 1);

        // Configurar Interrupter 0 del Event Ring.
        // Orden mandatorio por spec xHCI §5.5.2:
        //   1. Limpiar IP en IMAN (offset 0x20)
        //   2. Poner IMOD (offset 0x24)
        //   3. Escribir ERSTSZ (offset 0x28) = número de segmentos (1)
        //   4. Escribir ERSTBA (offsets 0x30/0x34)
        //   5. Escribir ERDP (offsets 0x38/0x3C)
        m.write_rt(0x20, 1); // IMAN: limpiar IP (bit 0), IE=0 de momento
        m.write_rt(0x24, 0); // IMOD=0 (máxima respuesta, sin moderación)
        m.write_rt(0x28, 1); // ERSTSZ = 1 segmento
        m.write_rt64(0x30, self.ev.erst_phys());
        let erdp = self.ev.erdp_phys();
        m.write_rt64(0x38, (erdp & !0xf) | 8);

        if self.msi_vector > 0 {
            m.write_rt(0x20, 3); // IMAN: IE=1, IP=1 (habilitar interrupciones)
        }

        // Iniciar controlador (Run)
        let mut usbcmd = m.read_op(0);
        usbcmd |= 1; // RS=1
        if self.msi_vector > 0 {
            usbcmd |= 1 << 2; // INTE
        }
        m.write_op(0, usbcmd);

        // Esperar a que el controlador salga de HCHalted (bit 0 de USBSTS=1 = halted).
        // NOTA: la condición es sts & 1 != 0 — el controlador está DETENIDO mientras ese bit esté en 1.
        let timeout_us = 100_000;
        let start = timer_now_us();
        let mut spins = 0u64;
        while !xhci_wait_expired(start, timeout_us, spins) {
            let sts = m.read_op(4);
            if (sts & 1) == 0 {
                // HCHalted=0 → controlador corriendo
                break;
            }
            spins = spins.saturating_add(1);
            spin_loop();
        }
        if spins >= xhci_wait_spin_limit(timeout_us) {
            warn!("[xhci] wait-HCHalted-clear salió por guard de spins (timer estancado?)");
        }
        {
            let sts = m.read_op(4);
            if (sts & 1) != 0 {
                error!(
                    "[xhci] controlador no arrancó (HCHalted persiste), USBSTS={:#010x}",
                    sts
                );
                self.dump_halt_diagnostics();
                return Err(DeviceError::NotReady);
            }
        }
        info!("[xhci] controlador en marcha");

        // Energía de puertos (Port Power, PP = bit 9).
        // Se pasa por `portsc_writeback` para nunca escribir 1 en PED (bit 1, RW1C) ni en
        // los bits de cambio, lo que borraría accidentalmente la habilitación del puerto.
        for p in 1..=self.max_ports {
            let off = 0x400 + (p as usize - 1) * 0x10;
            let sc = m.read_op(off);
            if (sc & (1 << 9)) == 0 {
                info!("[xhci] encendiendo puerto {} (PP=0 → 1)", p);
                m.write_op(off, portsc_writeback(sc, 1 << 9));
            }
        }
        // Pequeña espera tras dar energía (USB spec exige ≥100ms de VBUS estable antes de
        // que el dispositivo pueda responder; los devices gaming con firmware complejo lo necesitan).
        xhci_spin_delay_us(100_000);

        Ok(())
    }

    fn enumerate_root_hid(&mut self) {
        let maxp = self.max_ports;
        for port in 1..=maxp {
            if let Err(_e) = self.try_port_hid(port) {}
            // Procesar cambios de estado pendientes entre puertos para no acumularlos.
            self.drain_pending_port_changes();
        }
    }

    fn wait_port_ready(&self, off: usize, require_pr_clear: bool) -> Option<u8> {
        let m = &self.mmio;
        // Require multiple consecutive "ready" samples to filter transient link-state flaps.
        const STABLE_SAMPLES: u32 = 5;
        let mut stable = 0u32;
        let timeout_us = 1_000_000;
        let start = timer_now_us();
        let mut spins = 0u64;
        while !xhci_wait_expired(start, timeout_us, spins) {
            // Max 1s
            let s = m.read_op(off);
            let ccs = (s & 1) != 0;
            let ped = (s & (1 << 1)) != 0;
            let pr = (s & (1 << 4)) != 0;
            let spd = ((s >> 10) & 0x0f) as u8;
            let ready = ccs && spd != 0 && (ped || spd >= 4) && (!require_pr_clear || !pr);
            if ready {
                stable = stable.saturating_add(1);
                if stable >= STABLE_SAMPLES {
                    return Some(spd);
                }
            } else {
                stable = 0;
            }
            spins = spins.saturating_add(1);
            xhci_spin_delay_us(1000);
        }
        None
    }

    fn try_port_hid(&mut self, port: u8) -> DeviceResult<()> {
        let off = 0x400 + (port as usize - 1) * 0x10;
        let mut portsc = self.mmio.read_op(off);
        if (portsc & 1) == 0 {
            return Ok(());
        }
        if self.slot_on_root_port(port).is_some() {
            self.cleanup_port(port)?;
            portsc = self.mmio.read_op(off);
            if (portsc & 1) == 0 {
                return Ok(());
            }
        }
        // Ensure port power if the controller reports it as off.
        if (portsc & (1 << 9)) == 0 {
            info!("[xhci] puerto {}: PP=0, encendiendo", port);
            self.mmio.write_op(off, portsc_writeback(portsc, 1 << 9));
            xhci_spin_delay_us(100_000);
            portsc = self.mmio.read_op(off);
        }
        let pre_spd = ((portsc >> 10) & 0x0f) as u8;
        // needs_reset: solo si no hay velocidad asignada o el enlace no está habilitado.
        // spd >= 4 = SuperSpeed: PED no aplica de la misma forma; el port ya está listo.
        let ped = (portsc & (1 << 1)) != 0;
        let needs_reset = pre_spd == 0 || (!ped && pre_spd <= 3);

        // Reset del puerto (PR=1)
        if needs_reset {
            info!("[xhci] puerto {}: emitiendo reset", port);
            self.mmio.write_op(off, portsc_writeback(portsc, 1 << 4));

            // Espera robusta de reset (100ms)
            let mut success = false;
            let timeout_us = 100_000;
            let start = timer_now_us();
            let mut spins = 0u64;
            while !xhci_wait_expired(start, timeout_us, spins) {
                let s = self.mmio.read_op(off);
                if (s & (1 << 21)) != 0 || (s & (1 << 4)) == 0 {
                    success = true;
                    break;
                }
                spins = spins.saturating_add(1);
                spin_loop();
            }
            if !success {
                warn!("[xhci] puerto {}: timeout en reset", port);
            }
        }

        let spd = self.wait_port_ready(off, needs_reset).unwrap_or_else(|| {
            let s = self.mmio.read_op(off);
            ((s >> 10) & 0x0f) as u8
        });
        portsc = self.mmio.read_op(off);

        // Limpiar bits de cambio (CSC, PRC, etc) escribiendo 1; conservar PP, PED, etc.
        let clr = PORTSC_CHANGE_BITS;
        self.mmio.write_op(off, portsc_writeback(portsc, clr));

        // Pequeño delay tras reset para estabilización del link
        xhci_spin_delay_us(10_000);

        if spd == 0 || (self.mmio.read_op(off) & 1) == 0 {
            return Ok(());
        }
        match self.setup_device(DevTopo::root(port), spd) {
            Ok(()) => Ok(()),
            Err(first_err) => {
                warn!(
                    "[xhci] puerto {}: primer intento de enumeración falló ({:?}), reintentando",
                    port, first_err
                );
                self.cleanup_port(port)?;
                let mut s = self.mmio.read_op(off);
                if (s & 1) == 0 {
                    return Ok(());
                }
                self.mmio.write_op(off, portsc_writeback(s, 1 << 4));
                xhci_spin_delay_us(100_000);
                let spd_retry = self.wait_port_ready(off, true).unwrap_or_else(|| {
                    s = self.mmio.read_op(off);
                    ((s >> 10) & 0x0f) as u8
                });
                s = self.mmio.read_op(off);
                self.mmio.write_op(off, portsc_writeback(s, clr));
                xhci_spin_delay_us(50_000);
                if spd_retry == 0 || (self.mmio.read_op(off) & 1) == 0 {
                    return Ok(());
                }
                match self.setup_device(DevTopo::root(port), spd_retry) {
                    Ok(()) => Ok(()),
                    Err(second_err) => {
                        let _ = self.cleanup_port(port);
                        Err(second_err)
                    }
                }
            }
        }
    }

    fn setup_device(&mut self, topo: DevTopo, speed: u8) -> DeviceResult<()> {
        let port = topo.root_port;
        let cmd_trb_phys = self.cmd.push(trb_enable_slot())?;
        self.mmio.ring_db(0, 0); // Doorbell 0: Comando
        let slot = self.wait_cmd_phys_slot(cmd_trb_phys)?;
        if slot == 0 || slot as usize > self.max_slots as usize {
            // `slot` comes straight out of a Command Completion event. Only
            // zero was rejected, so a controller (or a stale/replayed event)
            // naming a slot above Max Slots panicked the kernel on the indexes
            // below -- `slot_speed`, `slot_topo` and `dev_ctx` are all sized
            // max_slots + 1. `handle_hid_transfer_side` already bounds it.
            error!(
                "[xhci] slot id {} out of range (max {})",
                slot, self.max_slots
            );
            return Err(DeviceError::IoError);
        }
        self.slot_speed[slot as usize] = speed;
        self.slot_topo[slot as usize] = Some(topo);

        let csz = self.context_size;
        let dev_sz = 32 * csz;
        let dev = DmaBuf::new(dev_sz, 64)?;
        // Flush DCBAA entry: el controlador lee este puntero vía DMA.
        // También hay que asegurarse de que el buffer de contexto esté en RAM (ceros).
        dev.flush(0, dev_sz);
        self.dcbaa.write_u64(slot as usize * 8, dev.sub_phys(0));
        self.dcbaa.flush(slot as usize * 8, 8);
        // If this slot somehow still carries a context (a retry that got the
        // same slot back without a Disable Slot in between), the controller
        // may still be reading it: abandon those pages rather than free them.
        if let Some(old) = self.dev_ctx[slot as usize].replace(dev) {
            warn!(
                "[xhci] slot={} reused with a live device context; leaking it",
                slot
            );
            old.leak();
        }

        let input_sz = 33 * csz;
        let ic = DmaBuf::new(input_sz, 64)?;
        ic.write_u32(4, 0x03);
        let s0 = csz;
        // Slot Context DW0 (§6.2.2):
        //   bits [19: 0] Route String = el puerto de cada nivel de hub, un
        //                               nibble por nivel; 0 en un puerto raíz
        //   bits [23:20] Speed        = PORTSC speed code
        //   bit  [25]    MTT          = Multi-Transaction Translator
        //   bit  [26]    Hub          = 0 (se pone en `configure_hub_slot`)
        //   bits [31:27] Ctx Entries  = 1 (solo EP0 inicial; se actualizará en Configure Endpoint)
        let slot_dw0 = (topo.route & 0x000f_ffff)
            | ((speed as u32) << 20)
            | (if topo.tt_multi { 1u32 << 25 } else { 0 })
            | (1u32 << 27); // Context Entries = 1
        ic.write_u32(s0, slot_dw0);
        // DW1: Max Exit Latency [15:0] = 0, Root Hub Port Number [23:16],
        // Number of Ports [31:24] = 0 (lo pone `configure_hub_slot`).
        ic.write_u32(s0 + 4, (port as u32) << 16);
        // DW2: TT Hub Slot ID [7:0], TT Port Number [15:8], TTT [17:16].
        // Sin esto, un teclado o un ratón de baja/plena velocidad detrás de un
        // hub de alta velocidad no enumera: el controlador no sabe por qué
        // traductor pasar sus transacciones.
        if topo.tt_slot != 0 {
            ic.write_u32(s0 + 8, (topo.tt_slot as u32) | ((topo.tt_port as u32) << 8));
        }
        let ep0 = 2 * csz;
        // xHCI PORTSC speed: 1=FS 2=LS 3=HS 4=SS Gen1 5=SS Gen2 …
        // Both FS(speed=1) and LS(speed=2) devices must start EP0 at 8 bytes until the
        // device descriptor tells us the real bMaxPacketSize0. Using 64 here breaks
        // enumeration for common HID keyboards/mice that come up on USB 1.x/2.0.
        let mps: u32 = match speed {
            1 | 2 => 8,
            3 => 64,
            4..=6 => 512,
            _ => return Err(DeviceError::InvalidParam),
        };
        ic.write_u32(ep0 + 4, (3 << 1) | EP_TYPE_CONTROL | (mps << 16));
        let ep0_ring = XferRing::new(32)?;
        let ep0_phys = ep0_ring.ring_phys();
        ic.write_u64(ep0 + 8, ep0_phys);
        ic.flush(0, input_sz);
        let ri = Self::ri(slot, 1);

        warn!(
            "[xhci] setup slot={} port={} route={:#x} tier={} tt=({},{}) speed={} csz={}",
            slot, port, topo.route, topo.depth, topo.tt_slot, topo.tt_port, speed, csz
        );
        warn!(
            "[xhci]   dcbaa[{}]={:#x}",
            slot,
            self.dcbaa.read_u64(slot as usize * 8)
        );
        warn!(
            "[xhci]   ic phys={:#x} ep0_ring_phys={:#x}",
            ic.sub_phys(0),
            ep0_phys
        );
        warn!(
            "[xhci]   ic.DW1(add)={:#010x} SlotCtx_DW0={:#010x} SlotCtx_DW1={:#010x}",
            ic.read_u32(4),
            ic.read_u32(s0),
            ic.read_u32(s0 + 4)
        );
        warn!(
            "[xhci]   EP0Ctx_DW1={:#010x} EP0_TRDeqPtr={:#x}",
            ic.read_u32(ep0 + 4),
            ic.read_u64(ep0 + 8)
        );

        if let Some(old) = self.xfer_rings[ri].replace(ep0_ring) {
            old.leak();
        }

        let p2 = self.cmd.push(trb_address_device(ic.sub_phys(0), slot))?;
        self.mmio.ring_db(0, 0);
        // `ic` must outlive the wait: the controller reads it by DMA. If the
        // command never completes we do not know when it stops, so the pages
        // are abandoned rather than returned to the allocator.
        if let Err(e) = self.wait_cmd_phys(p2) {
            ic.leak();
            return Err(e);
        }
        drop(ic);

        warn!(
            "[xhci] Address Device completado slot={} USBSTS={:#010x}",
            slot,
            self.mmio.read_op(4)
        );

        // Pequeña pausa tras Address Device: algunos dispositivos FS/LS necesitan
        // tiempo para procesar el SET_ADDRESS y estar listos en la nueva dirección.
        {
            xhci_spin_delay_us(2_000); // 2ms
        }

        // Invalidar la caché del descriptor buffer antes de pasarlo al controlador
        // (el controlador escribirá en él via DMA; queremos ver los datos frescos).
        let desc = DmaBuf::new(64, 64)?;
        desc.flush(0, 64);
        warn!(
            "[xhci] GET_DESCRIPTOR slot={} desc.phys={:#x} ep0_ring.phys={:#x}",
            slot,
            desc.phys,
            self.xfer_rings
                .get(ri)
                .and_then(|o| o.as_ref())
                .map(|r| r.buf.phys)
                .unwrap_or(0)
        );
        if let Err(e) =
            self.ep0_control_in(slot, trb_setup(0x80, 0x06, 0x0100, 0, 18, 3), &desc, 18)
        {
            // Same reasoning as `ic` above: a control transfer that did not
            // complete leaves the controller free to write here later.
            desc.leak();
            return Err(e);
        }

        // Invalidar caché del buffer de descriptor para ver los datos escritos por DMA.
        desc.flush(0, 64);

        // Leer bMaxPacketSize0 (byte 7) y actualizar contexto de EP0
        let mut raw_desc = [0u8; 18];
        desc.read_into(0, &mut raw_desc);
        let vid = u16::from_le_bytes([raw_desc[8], raw_desc[9]]);
        let pid = u16::from_le_bytes([raw_desc[10], raw_desc[11]]);
        info!(
            "[xhci] puerto {}: dispositivo detectado VID={:04x} PID={:04x} class={:02x}",
            port, vid, pid, raw_desc[4]
        );

        let real_mps = raw_desc[7] as u32;
        if real_mps != mps && real_mps >= 8 {
            let ic_upd = DmaBuf::new(input_sz, 64)?;
            // Add EP0 (A1) only, like `xhci_check_maxpacket` in Linux: with A0
            // clear the Slot Context is not evaluated, so we cannot disturb
            // the device address the controller just assigned.
            ic_upd.write_u32(4, 0x02);
            // Copiar contexto actual
            if let Some(dev_ctx) = self.dev_ctx[slot as usize].as_ref() {
                // Invalidar caché antes de leer datos escritos por el controlador via DMA.
                dev_ctx.flush(0, dev_sz);
                // Copiar Slot Context (dev index 0 -> input index 1)
                for i in 0..(csz / 4) {
                    ic_upd.write_u32(csz + i * 4, dev_ctx.read_u32(i * 4));
                }
                // Copiar EP0 Context (dev index 1 -> input index 2)
                for i in 0..(csz / 4) {
                    ic_upd.write_u32(2 * csz + i * 4, dev_ctx.read_u32(csz + i * 4));
                }
            }
            // Actualizar MPS en el contexto de EP0
            let ep0_dw1 = ic_upd.read_u32(ep0 + 4);
            ic_upd.write_u32(ep0 + 4, (ep0_dw1 & 0x0000FFFF) | (real_mps << 16));

            // Flush BEFORE the doorbell: the controller reads the input
            // context by DMA as soon as it is rung.
            ic_upd.flush(0, input_sz);
            let p_upd = self
                .cmd
                .push(trb_evaluate_context(ic_upd.sub_phys(0), slot))?;
            self.mmio.ring_db(0, 0);
            if self.wait_cmd_phys(p_upd).is_err() {
                warn!(
                    "[xhci] slot={} Evaluate Context for bMaxPacketSize0={} failed; \
                     EP0 stays at {}",
                    slot, real_mps, mps
                );
                ic_upd.leak();
            }
        }

        let dev_class = raw_desc[4];
        self.devs.retain(|d| d.slot != slot);
        self.devs.push(UsbDev {
            slot,
            topo,
            speed,
            vid,
            pid,
            class: dev_class,
            subclass: raw_desc[5],
            proto: raw_desc[6],
            ifaces: Vec::new(),
            ifaces_dropped: 0,
        });
        self.setup_hid_from_config(slot, csz, port, vid, pid)?;
        if dev_class == USB_CLASS_HUB {
            self.setup_hub(slot, csz, topo, speed)?;
        }

        Ok(())
    }

    /// Slot del dispositivo que está directamente en el puerto raíz `port`.
    fn slot_on_root_port(&self, port: u8) -> Option<u8> {
        (1..=self.max_slots).find(|&s| {
            self.slot_topo
                .get(s as usize)
                .copied()
                .flatten()
                .is_some_and(|t| t.root_port == port && t.depth == 0)
        })
    }

    /// Slot del dispositivo enchufado al puerto `port` del hub `hub_slot`.
    fn slot_under(&self, hub_slot: u8, port: u8) -> Option<u8> {
        (1..=self.max_slots).find(|&s| {
            self.slot_topo
                .get(s as usize)
                .copied()
                .flatten()
                .is_some_and(|t| t.parent_slot == hub_slot && t.parent_port == port)
        })
    }

    /// Petición de clase hub sin etapa de datos: `SET_FEATURE` o
    /// `CLEAR_FEATURE` sobre un puerto aguas abajo. `bmRequestType = 0x23`
    /// (host->device, clase, destinatario «Other»).
    fn hub_port_feature(
        &mut self,
        slot: u8,
        set: bool,
        feature: u16,
        port: u8,
    ) -> DeviceResult<()> {
        let breq = if set { 0x03 } else { 0x01 };
        self.ep0_control_out0(
            slot,
            trb_setup(0x23, breq, feature, port as u16, 0, 0),
            true,
        )
    }

    /// `GET_STATUS` de un puerto del hub: devuelve `(wPortStatus, wPortChange)`.
    fn hub_port_status(&mut self, slot: u8, port: u8) -> DeviceResult<(u16, u16)> {
        let buf = DmaBuf::new(64, 64)?;
        buf.flush(0, 64); // evict stale zeros before DMA
        if let Err(e) =
            self.ep0_control_in(slot, trb_setup(0xa3, 0x00, 0, port as u16, 4, 3), &buf, 4)
        {
            // El controlador puede escribir aquí más tarde si la transferencia
            // no llegó a completarse: se abandonan las páginas.
            buf.leak();
            return Err(e);
        }
        buf.flush(0, 64); // invalidate so CPU reads fresh DMA data
        let mut raw = [0u8; 4];
        buf.read_into(0, &mut raw);
        Ok((
            u16::from_le_bytes([raw[0], raw[1]]),
            u16::from_le_bytes([raw[2], raw[3]]),
        ))
    }

    /// Lee el descriptor de hub. `bmRequestType = 0xa0` (device->host, clase,
    /// destinatario «Device»).
    fn hub_descriptor(&mut self, slot: u8, speed: u8) -> DeviceResult<HubInfo> {
        let dtype = if speed >= SPEED_SUPER {
            USB_DESC_SS_HUB
        } else {
            USB_DESC_HUB
        };
        let buf = DmaBuf::new(64, 64)?;
        buf.flush(0, 64);
        if let Err(e) = self.ep0_control_in(
            slot,
            trb_setup(0xa0, 0x06, (dtype as u16) << 8, 0, 15, 3),
            &buf,
            15,
        ) {
            buf.leak();
            return Err(e);
        }
        buf.flush(0, 64);
        let mut raw = [0u8; 15];
        buf.read_into(0, &mut raw);
        parse_hub_descriptor(&raw).ok_or_else(|| {
            warn!(
                "[xhci] hub slot={}: descriptor de hub ilegible ({:?})",
                slot, raw
            );
            DeviceError::InvalidParam
        })
    }

    /// Le dice al controlador que este slot es un hub. Sin el bit Hub, el
    /// número de puertos y el think time, no ruta nada aguas abajo (§4.3.3), y
    /// un hijo que lo nombre como su TT recibe un Parameter Error.
    ///
    /// Va por Configure Endpoint y no por Evaluate Context: este último solo
    /// evalúa Max Exit Latency e Interrupter Target (§4.6.7).
    fn configure_hub_slot(&mut self, slot: u8, csz: usize, info: &HubInfo) -> DeviceResult<()> {
        let input_sz = 33 * csz;
        let ic = DmaBuf::new(input_sz, 64)?;
        ic.write_u32(0, 0); // Drop Context flags: nada
        ic.write_u32(4, 0x01); // Add Context flags: A0 (Slot Context)
        let dev_sz = 32 * csz;
        {
            let Some(dev_ctx) = self.dev_ctx[slot as usize].as_ref() else {
                return Err(DeviceError::InvalidParam);
            };
            // Invalidar antes de leer lo que el controlador escribió por DMA.
            dev_ctx.flush(0, dev_sz);
            for i in 0..(csz / 4) {
                ic.write_u32(csz + i * 4, dev_ctx.read_u32(i * 4));
            }
        }
        let s0 = csz;
        ic.write_u32(s0, ic.read_u32(s0) | (1 << 26)); // Hub = 1
        ic.write_u32(
            s0 + 4,
            (ic.read_u32(s0 + 4) & 0x00ff_ffff) | ((info.ports as u32) << 24),
        );
        ic.write_u32(
            s0 + 8,
            (ic.read_u32(s0 + 8) & !(3 << 16)) | ((info.think_time as u32 & 3) << 16),
        );
        // Flush ANTES del doorbell: el controlador lee el input context por DMA
        // en cuanto se le llama.
        ic.flush(0, input_sz);
        let p = self
            .cmd
            .push(trb_configure_endpoint(ic.sub_phys(0), slot))?;
        self.mmio.ring_db(0, 0);
        if let Err(e) = self.wait_cmd_phys(p) {
            // Si el comando no completó no se sabe cuándo deja de leerlo.
            ic.leak();
            return Err(e);
        }
        Ok(())
    }

    /// Enciende todos los puertos de un hub recién direccionado y enumera lo
    /// que ya esté enchufado en ellos.
    fn setup_hub(&mut self, slot: u8, csz: usize, topo: DevTopo, speed: u8) -> DeviceResult<()> {
        let info = self.hub_descriptor(slot, speed)?;
        self.configure_hub_slot(slot, csz, &info)?;
        info!(
            "[xhci] hub slot={} con {} puertos (route={:#x}, nivel {}, think_time={}, \
             power_good={}ms)",
            slot, info.ports, topo.route, topo.depth, info.think_time, info.power_good_ms
        );
        self.hubs.retain(|h| h.slot != slot);
        self.hubs.push(HubDev {
            slot,
            speed,
            ports: info.ports,
            multi_tt: info.multi_tt,
            ep_dci: 0,
            scan_last_us: timer_now_us(),
            scan_due: false,
            buf: None,
            change_len: 0,
        });
        // Armarlo antes de encender los puertos: así los cambios de los
        // dispositivos que ya estuvieran enchufados llegan por el endpoint en
        // vez de depender del primer barrido.
        if let Err(e) = self.arm_hub_status_endpoint(slot, csz, info.ports) {
            warn!(
                "[xhci] hub slot={}: sin endpoint de cambio de estado ({:?}); se queda con \
                 el sondeo cada {} ms",
                slot,
                e,
                HUB_SCAN_PERIOD_US / 1000
            );
        }
        for p in 1..=info.ports {
            if let Err(e) = self.hub_port_feature(slot, true, HUB_FEAT_PORT_POWER, p) {
                warn!(
                    "[xhci] hub slot={} puerto {}: PORT_POWER falló ({:?})",
                    slot, p, e
                );
            }
        }
        xhci_spin_delay_us(info.power_good_ms as u64 * 1000);
        for p in 1..=info.ports {
            if let Err(e) = self.try_hub_port(slot, p) {
                warn!(
                    "[xhci] hub slot={} puerto {}: enumeración falló ({:?})",
                    slot, p, e
                );
            }
        }
        // El barrido periódico acaba de hacerse aquí: no repetirlo enseguida.
        let now = timer_now_us();
        if let Some(h) = self.hubs.iter_mut().find(|h| h.slot == slot) {
            h.scan_last_us = now;
            h.scan_due = false;
        }
        Ok(())
    }

    /// Enumera (o limpia) lo que haya en el puerto `port` del hub `hub_slot`.
    fn try_hub_port(&mut self, hub_slot: u8, port: u8) -> DeviceResult<()> {
        let Some((hub_speed, _, hub_multi_tt)) =
            self.hubs.iter().find(|h| h.slot == hub_slot).map(hub_facts)
        else {
            return Ok(());
        };
        let Some(hub_topo) = self.slot_topo.get(hub_slot as usize).copied().flatten() else {
            return Ok(());
        };
        let (status, change) = self.hub_port_status(hub_slot, port)?;
        // Reconocer los cambios ANTES de enumerar, y solo esos, por la misma
        // razón que en `handle_port_status_change`: la enumeración tarda, y un
        // desenchufe que ocurra mientras corre no se puede perder.
        for &(bit, feat) in hub_port_changes(hub_speed) {
            if change & bit != 0 {
                let _ = self.hub_port_feature(hub_slot, false, feat, port);
            }
        }
        let connected = status & HUB_PORT_CONNECTION != 0;
        let existing = self.slot_under(hub_slot, port);
        match (connected, existing) {
            (false, Some(child)) => {
                info!(
                    "[xhci] hub slot={} puerto {}: desconexión, liberando el slot {}",
                    hub_slot, port, child
                );
                return self.cleanup_slot_tree(child);
            }
            (false, None) => return Ok(()),
            // Ya enumerado y sigue ahí.
            (true, Some(_)) => return Ok(()),
            (true, None) => {}
        }
        // `PORT_RESET` tambien en un hub SuperSpeed: ahi es el que arranca el
        // entrenamiento del enlace. `BH_PORT_RESET` es un reset en caliente,
        // para recuperarse, no para enumerar por primera vez.
        self.hub_port_feature(hub_slot, true, HUB_FEAT_PORT_RESET, port)?;
        let mut status = 0u16;
        let mut enabled = false;
        let start = timer_now_us();
        let mut spins = 0u64;
        while !xhci_wait_expired(start, HUB_RESET_TIMEOUT_US, spins) {
            xhci_spin_delay_us(10_000);
            spins = spins.saturating_add(1);
            let (s, c) = self.hub_port_status(hub_slot, port)?;
            status = s;
            for &(bit, feat) in hub_port_changes(hub_speed) {
                if c & bit != 0 {
                    let _ = self.hub_port_feature(hub_slot, false, feat, port);
                }
            }
            if s & HUB_PORT_CONNECTION == 0 {
                // Se fue mientras se reseteaba.
                return Ok(());
            }
            if s & HUB_PORT_RESET == 0 && s & HUB_PORT_ENABLE != 0 {
                enabled = true;
                break;
            }
        }
        if !enabled {
            warn!(
                "[xhci] hub slot={} puerto {}: el reset no habilitó el puerto \
                 (wPortStatus={:#06x})",
                hub_slot, port, status
            );
            return Ok(());
        }
        xhci_spin_delay_us(HUB_RESET_RECOVERY_US);
        let speed = hub_port_speed(hub_speed, status);
        let Some(child) = hub_topo.child(hub_slot, hub_speed, port, speed, hub_multi_tt) else {
            warn!(
                "[xhci] hub slot={} puerto {}: queda más allá de los {} niveles que un \
                 route string puede nombrar, no se enumera",
                hub_slot, port, USB_MAX_TIERS
            );
            return Ok(());
        };
        info!(
            "[xhci] hub slot={} puerto {}: dispositivo a velocidad {} (route={:#x}, nivel {})",
            hub_slot, port, speed, child.route, child.depth
        );
        self.setup_device(child, speed)
    }

    /// Busca el endpoint de interrupción IN de la interfaz de clase hub en el
    /// descriptor de configuración. Un hub tiene exactamente uno (USB 2.0
    /// §11.12.1) y es por donde avisa de sus cambios de puerto.
    fn hub_status_endpoint(&mut self, slot: u8) -> DeviceResult<(u8, u16, u8)> {
        let sniff = DmaBuf::new(64, 64)?;
        sniff.flush(0, 64);
        if let Err(e) = self.ep0_control_in(slot, trb_setup(0x80, 0x06, 0x0200, 0, 9, 3), &sniff, 9)
        {
            sniff.leak();
            return Err(e);
        }
        sniff.flush(0, 64);
        let mut hdr = [0u8; 9];
        sniff.read_into(0, &mut hdr);
        let total = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
        if !(9..=8192).contains(&total) {
            return Err(DeviceError::InvalidParam);
        }
        let buf_len = (total.div_ceil(64) * 64).max(64);
        let cfgb = DmaBuf::new(buf_len, 64)?;
        cfgb.flush(0, buf_len);
        if let Err(e) = self.ep0_control_in(
            slot,
            trb_setup(0x80, 0x06, 0x0200, 0, total as u16, 3),
            &cfgb,
            total as u32,
        ) {
            cfgb.leak();
            return Err(e);
        }
        cfgb.flush(0, buf_len);
        let mut raw = alloc::vec![0u8; total];
        cfgb.read_into(0, &mut raw[..total]);
        class_int_in_endpoint(&raw, USB_CLASS_HUB).ok_or(DeviceError::NotSupported)
    }

    /// Arma el endpoint de cambio de estado de un hub ya configurado.
    ///
    /// Si falla, el hub se queda con el sondeo de [`Self::scan_hubs_if_due`]:
    /// más lento, pero un hub mudo sería peor.
    fn arm_hub_status_endpoint(&mut self, slot: u8, csz: usize, ports: u8) -> DeviceResult<()> {
        let (ep_addr, mps, interval) = self.hub_status_endpoint(slot)?;
        // El mapa de bits nunca pasa de 16 bytes (15 puertos + el bit del hub),
        // pero se le deja el paquete entero del endpoint para que un hub que
        // mande más no desborde a Babble.
        let len = hub_change_bytes(ports).max(mps as usize).max(2);
        let dci =
            self.configure_endpoint(slot, csz, ep_addr, EP_TYPE_INT_IN, mps, interval, len, 16)?;
        let buf = DmaBuf::new(len, 64)?;
        // Evict the zeroing `DmaBuf::new` just did before the controller starts
        // DMA-ing into it, igual que los búferes de informe HID.
        buf.flush(0, len);
        let phys = buf.sub_phys(0);
        {
            let ring = self
                .xfer_rings
                .get_mut(Self::ri(slot, dci))
                .and_then(|o| o.as_mut())
                .ok_or(DeviceError::NotSupported)?;
            ring.push(trb_normal(phys, len as u16, true))?;
        }
        if let Some(h) = self.hubs.iter_mut().find(|h| h.slot == slot) {
            h.ep_dci = dci;
            h.change_len = len;
            if let Some(old) = h.buf.replace(buf) {
                old.leak();
            }
        } else {
            buf.leak();
            return Err(DeviceError::InvalidParam);
        }
        self.mmio.ring_db(slot, dci);
        info!(
            "[xhci] hub slot={}: cambios de puerto por el endpoint dci={} ({} bytes de mapa \
             de bits, intervalo {})",
            slot, dci, len, interval
        );
        Ok(())
    }

    /// Vuelve a armar el único TRB del endpoint de cambio de estado de un hub.
    fn rearm_hub_status_trb(&mut self, slot: u8) {
        let Some((dci, len, phys)) = self.hubs.iter().find(|h| h.slot == slot).and_then(|h| {
            h.buf
                .as_ref()
                .map(|b| (h.ep_dci, h.change_len, b.sub_phys(0)))
        }) else {
            return;
        };
        if dci == 0 {
            return;
        }
        let ridx = Self::ri(slot, dci);
        if let Some(r) = self.xfer_rings.get_mut(ridx).and_then(|o| o.as_mut()) {
            if r.push(trb_normal(phys, len as u16, true)).is_err() {
                warn!(
                    "[xhci] hub slot={}: el anillo del endpoint de estado está lleno; el \
                     sondeo cubre los cambios",
                    slot
                );
                return;
            }
        } else {
            return;
        }
        fence(Ordering::SeqCst);
        self.mmio.ring_db(slot, dci);
    }

    /// Atiende una Transfer Event del endpoint de cambio de estado de un hub:
    /// apunta los puertos que el mapa de bits señala y vuelve a armar el TRB.
    ///
    /// Los puertos no se tocan aquí: esto corre dentro de `pop_ev` y enumerar
    /// emite comandos que esperan en el anillo de eventos.
    fn handle_hub_status_event(&mut self, slot: u8, ev: &Trb, cc: u32) -> bool {
        let Some((dci, len)) = self
            .hubs
            .iter()
            .find(|h| h.slot == slot)
            .map(|h| (h.ep_dci, h.change_len))
        else {
            return false;
        };
        let ridx = Self::ri(slot, dci);
        if let Some(r) = self.xfer_rings.get_mut(ridx).and_then(|o| o.as_mut()) {
            r.advance_dequeue(1);
        }
        if cc != TRB_CC_SUCCESS && cc != TRB_CC_SHORT {
            if cc_halts_endpoint(cc) {
                let stalled = cc == 6;
                if let Some(e) = self
                    .pending_ep_resets
                    .iter_mut()
                    .find(|(sl, d, _)| *sl == slot && *d == dci)
                {
                    e.2 |= stalled;
                } else {
                    self.pending_ep_resets.push((slot, dci, stalled));
                }
            } else {
                warn!(
                    "[xhci] hub slot={} dci={} estado cc={} (el endpoint no está halted)",
                    slot, dci, cc
                );
            }
            self.rearm_hub_status_trb(slot);
            // Un barrido cubre lo que ese informe perdido traía.
            if let Some(h) = self.hubs.iter_mut().find(|h| h.slot == slot) {
                h.scan_due = true;
            }
            return true;
        }
        // Residual en los 24 bits bajos: lo que NO se transfirió (§6.4.2.1).
        let actual = len.saturating_sub((ev.status & 0x00ff_ffff) as usize);
        let mut bitmap = [0u8; 16];
        let n = actual.min(bitmap.len());
        let ports = self
            .hubs
            .iter()
            .find(|h| h.slot == slot)
            .map(|h| h.ports)
            .unwrap_or(0);
        if let Some(buf) = self
            .hubs
            .iter()
            .find(|h| h.slot == slot)
            .and_then(|h| h.buf.as_ref())
        {
            // Invalidar para ver lo que el controlador acaba de escribir.
            buf.flush(0, len);
            buf.read_into(0, &mut bitmap[..n]);
        }
        for port in hub_changed_ports(&bitmap[..n], ports) {
            if !self.pending_hub_ports.contains(&(slot, port)) {
                self.pending_hub_ports.push((slot, port));
            }
        }
        self.rearm_hub_status_trb(slot);
        true
    }

    /// Atiende los puertos que los endpoints de estado han señalado.
    fn drain_pending_hub_ports(&mut self) {
        for _ in 0..64 {
            let Some((slot, port)) = self.pending_hub_ports.first().copied() else {
                return;
            };
            self.pending_hub_ports.remove(0);
            if let Err(e) = self.try_hub_port(slot, port) {
                warn!(
                    "[xhci] hub slot={} puerto {}: el cambio señalado no se pudo atender ({:?})",
                    slot, port, e
                );
            }
        }
    }

    /// Una transferencia bulk, de principio a fin, y los bytes que movio.
    ///
    /// Sincrona: empuja un Normal TRB, toca la campana y espera su Transfer
    /// Event. Es la misma forma que `wait_ep0_status_any` y por la misma razon
    /// --hay que atender el anillo de eventos mientras se espera, porque por el
    /// pasan tambien los informes de los teclados-- asi que no puede correr
    /// anidada dentro de `pop_ev`.
    fn bulk_transfer(&mut self, slot: u8, dci: u8, buf_phys: u64, len: u32) -> DeviceResult<u32> {
        let ridx = Self::ri(slot, dci);
        let trb_phys = {
            let ring = self
                .xfer_rings
                .get_mut(ridx)
                .and_then(|o| o.as_mut())
                .ok_or(DeviceError::NotSupported)?;
            // Una transferencia bulk puede ser de longitud cero: un CBW sin
            // fase de datos no lleva TRB de datos, pero el CSW si.
            ring.push(trb_normal(buf_phys, len as u16, true))?
        };
        if let Some(r) = self.xfer_rings.get(ridx).and_then(|o| o.as_ref()) {
            r.buf.flush(0, r.buf.len);
        }
        fence(Ordering::SeqCst);
        self.mmio.ring_db(slot, dci);

        let start = timer_now_us();
        let mut spins = 0u64;
        while !xhci_wait_expired(start, BULK_TIMEOUT_US, spins) {
            if let Some(ev) = self.pop_ev(None) {
                if (ev.ctrl >> 10) & 0x3f == 32
                    && ((ev.ctrl >> 24) & 0xff) as u8 == slot
                    && ((ev.ctrl >> 16) & 0x1f) as u8 == dci
                    // VirtualBox apunta a veces al TRB SIGUIENTE al completado,
                    // igual que en EP0.
                    && (ev.p == trb_phys || ev.p == trb_phys.wrapping_add(16))
                {
                    if let Some(r) = self.xfer_rings.get_mut(ridx).and_then(|o| o.as_mut()) {
                        r.advance_dequeue(1);
                    }
                    let cc = (ev.status >> 24) & 0xff;
                    if cc == TRB_CC_SUCCESS || cc == TRB_CC_SHORT {
                        // Residual en los 24 bits bajos: lo que NO se movio.
                        return Ok(len.saturating_sub(ev.status & 0x00ff_ffff));
                    }
                    // Un STALL en un endpoint bulk es como el dispositivo dice
                    // «esa orden no» (USB MSC BOT §6.7.2): se le quita el halt
                    // y se sigue, que es lo que espera el CSW de despues.
                    if cc == 6 {
                        let _ = self.clear_endpoint_halt(slot, dci);
                    }
                    let _ = self.reset_endpoint_and_dequeue(slot, dci);
                    warn!("[xhci] bulk slot={} dci={} cc={}", slot, dci, cc);
                    return Err(DeviceError::IoError);
                }
            }
            spins = spins.saturating_add(1);
            spin_loop();
        }
        // Igual que en EP0: adelantar el dequeue del software por encima del TD
        // que nunca completo, o el siguiente timbre lo repite y su respuesta
        // tardia satisface la espera de la orden de despues.
        if let Some(r) = self.xfer_rings.get_mut(ridx).and_then(|o| o.as_mut()) {
            r.advance_dequeue(1);
        }
        let _ = self.reset_endpoint_and_dequeue(slot, dci);
        warn!("[xhci] bulk slot={} dci={} timeout", slot, dci);
        Err(DeviceError::IoError)
    }

    /// Una orden SCSI completa sobre Bulk-Only Transport: CBW, fase de datos y
    /// CSW. Devuelve los bytes de datos que llegaron.
    ///
    /// `Err` cuando el transporte fallo; `Ok` con el `status` del CSW distinto
    /// de cero cuando la orden llego y la unidad la rechazo, que son dos cosas
    /// muy distintas y mezclarlas convierte «no hay tarjeta metida» en «el
    /// dispositivo no funciona».
    /// Devuelve el bufer de rebote a su unidad, o lo pierde si la unidad ya no
    /// esta en la lista.
    ///
    /// Indexar seria un panico el dia en que la lista pueda cambiar durante una
    /// orden, y liberar el bufer cuando no hay a quien devolverlo es peor que
    /// perderlo: el controlador puede seguir mirandolo.
    fn return_bounce(&mut self, idx: usize, bounce: DmaBuf) {
        match self.mscs.get_mut(idx) {
            Some(m) => m.bounce = Some(bounce),
            None => bounce.leak(),
        }
    }

    fn msc_command(
        &mut self,
        idx: usize,
        cmd: &[u8],
        data: Option<(&DmaBuf, u32, bool)>,
    ) -> DeviceResult<(u32, u8)> {
        let (slot, dci_in, dci_out, tag) = {
            let m = self.mscs.get_mut(idx).ok_or(DeviceError::InvalidParam)?;
            let tag = m.next_tag;
            m.next_tag = m.next_tag.wrapping_add(1);
            (m.slot, m.dci_in, m.dci_out, tag)
        };
        let (data_len, dir_in) = data.map(|(_, l, i)| (l, i)).unwrap_or((0, false));
        let cbw = bot_cbw(tag, data_len, dir_in, 0, cmd).ok_or(DeviceError::InvalidParam)?;

        let wrap = DmaBuf::new(64, 64)?;
        for (i, b) in cbw.iter().enumerate() {
            // `DmaBuf` escribe en dwords; el CBW no esta alineado a cuatro.
            let off = i & !3;
            let mut w = wrap.read_u32(off).to_le_bytes();
            w[i & 3] = *b;
            wrap.write_u32(off, u32::from_le_bytes(w));
        }
        wrap.flush(0, 64);
        if self
            .bulk_transfer(slot, dci_out, wrap.sub_phys(0), BOT_CBW_LEN as u32)
            .is_err()
        {
            wrap.leak();
            return Err(DeviceError::IoError);
        }

        let mut moved = 0u32;
        if let Some((buf, len, is_in)) = data {
            if len > 0 {
                let dci = if is_in { dci_in } else { dci_out };
                if !is_in {
                    buf.flush(0, len as usize);
                }
                // Un STALL aqui no es el fin: la unidad puede haber decidido
                // mandar menos de lo pedido y el CSW de despues lo explica.
                moved = self
                    .bulk_transfer(slot, dci, buf.sub_phys(0), len)
                    .unwrap_or(0);
                if is_in {
                    buf.flush(0, len as usize);
                }
            }
        }

        wrap.flush(0, 64);
        if self
            .bulk_transfer(slot, dci_in, wrap.sub_phys(0), BOT_CSW_LEN as u32)
            .is_err()
        {
            wrap.leak();
            return Err(DeviceError::IoError);
        }
        wrap.flush(0, 64);
        let mut raw = [0u8; BOT_CSW_LEN];
        wrap.read_into(0, &mut raw);
        let Some(csw) = bot_parse_csw(&raw, tag) else {
            warn!(
                "[xhci] msc slot={} CSW invalido o de otra orden (tag esperado {}): {:?}",
                slot, tag, raw
            );
            return Err(DeviceError::IoError);
        };
        Ok((moved, csw.status))
    }

    /// Una tanda de READ(10) o WRITE(10) sobre la unidad `disk_id`, partida en
    /// las vueltas que quepan en un TRB.
    ///
    /// `first_block` y la longitud van en bloques del DISPOSITIVO, no en
    /// sectores de 512: esa traduccion la hace [`UsbDisk`], que es quien sabe
    /// que el resto del kernel cuenta de 512 en 512.
    fn msc_rw(&mut self, disk_id: u64, first_block: u64, mut data: MscData<'_>) -> DeviceResult {
        let Some(idx) = self.mscs.iter().position(|m| m.disk_id == disk_id) else {
            // La unidad se fue mientras alguien tenia el disco abierto. Eso no
            // es un error del llamante y no es lo mismo que una E/S fallida.
            return Err(DeviceError::NotReady);
        };
        let (slot, bs, dev_blocks) = {
            let m = &self.mscs[idx];
            let cap = m.capacity.ok_or(DeviceError::NotReady)?;
            (
                m.slot,
                cap.block_size as usize,
                cap.last_lba.saturating_add(1),
            )
        };
        if bs == 0 {
            return Err(DeviceError::NotSupported);
        }
        let len = data.len();
        if len == 0 || !len.is_multiple_of(bs) {
            return Err(DeviceError::InvalidParam);
        }
        let total = len / bs;
        // El final tiene que caber en el disco. Sin esto, leer el ultimo sector
        // con un bufer de mas se convierte en un LBA fuera de rango, y la
        // unidad contesta un error que arriba se lee como disco roto en vez de
        // como peticion mal hecha.
        if first_block
            .checked_add(total as u64)
            .is_none_or(|end| end > dev_blocks)
        {
            return Err(DeviceError::InvalidParam);
        }

        let bounce = match self.mscs[idx].bounce.take() {
            Some(b) => b,
            None => DmaBuf::new(MSC_BOUNCE_BYTES, 64)?,
        };
        // Lo que cabe en una vuelta: el campo del TRB y el bufer, los dos
        // redondeados HACIA ABAJO a un bloque entero, porque medio bloque no es
        // una peticion valida.
        let per_turn = ((MSC_MAX_TRB_LEN as usize).min(bounce.len) / bs).max(1);
        let write = data.is_write();
        let op = if write { SCSI_WRITE_10 } else { SCSI_READ_10 };

        let mut done = 0usize; // en bloques del dispositivo
        while done < total {
            let turn = per_turn.min(total - done);
            let bytes = turn * bs;
            let off = done * bs;

            if let MscData::Write(src) = &data {
                if bounce.write_from(0, &src[off..off + bytes]) != bytes {
                    self.return_bounce(idx, bounce);
                    return Err(DeviceError::BufferTooSmall);
                }
            }
            let Some(cdb) = scsi_rw10(op, first_block + done as u64, turn as u32) else {
                self.return_bounce(idx, bounce);
                return Err(DeviceError::InvalidParam);
            };
            match self.msc_command(idx, &cdb, Some((&bounce, bytes as u32, !write))) {
                Ok((moved, 0)) if moved as usize == bytes => {}
                Ok((moved, status)) => {
                    // La orden llego y la unidad no la hizo, o la hizo a
                    // medias. Un `Ok` aqui dejaria datos sin escribir, o un
                    // bufer medio lleno de ceros creyendose un sector leido.
                    warn!(
                        "[xhci] disco slot={} {} lba={} bloques={}: CSW status={} movidos={}/{}",
                        slot,
                        if write { "WRITE" } else { "READ" },
                        first_block + done as u64,
                        turn,
                        status,
                        moved,
                        bytes
                    );
                    self.return_bounce(idx, bounce);
                    return Err(DeviceError::IoError);
                }
                Err(e) => {
                    // El transporte fallo: el controlador puede seguir
                    // escribiendo en el bufer de rebote, asi que ese bufer no
                    // vuelve ni a la unidad ni al asignador. Mismo criterio que
                    // el resto de la DMA de este driver.
                    bounce.leak();
                    return Err(e);
                }
            }
            if let MscData::Read(dst) = &mut data {
                bounce.read_into(0, &mut dst[off..off + bytes]);
            }
            done += turn;
        }
        self.return_bounce(idx, bounce);
        Ok(())
    }

    /// SYNCHRONIZE CACHE(10): que la unidad baje su cache volatil al medio.
    fn msc_flush(&mut self, disk_id: u64) -> DeviceResult {
        let Some(idx) = self.mscs.iter().position(|m| m.disk_id == disk_id) else {
            return Err(DeviceError::NotReady);
        };
        match self.msc_command(idx, &scsi_sync_cache10(), None) {
            // Un pendrive sin cache de escritura contesta que no conoce la
            // orden, y eso no es un fallo: no tiene nada que bajar. Por eso el
            // status distinto de cero no se convierte en error aqui, al
            // contrario que en una lectura.
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Prepara una interfaz de almacenamiento masivo y le pregunta quien es.
    ///
    /// Tampoco la da de alta como disco aqui: solo la apunta en
    /// `pending_disk_regs` cuando contesto su capacidad, y el alta la hace
    /// `drain_disk_changes` fuera de este cerrojo. Pase lo que pase con el
    /// alta, la unidad sale en `/proc/usbhid` con su nombre y su capacidad,
    /// que es lo que contesta «¿ve el sistema mi pendrive?».
    fn setup_mass_storage(
        &mut self,
        slot: u8,
        csz: usize,
        iface: u8,
        subclass: u8,
        proto: u8,
        cfg_desc: &[u8],
    ) -> DeviceResult<()> {
        if proto != MSC_PROTO_BULK_ONLY {
            warn!(
                "[xhci] msc slot={} iface={}: bInterfaceProtocol={:#04x} no es Bulk-Only",
                slot, iface, proto
            );
            return Err(DeviceError::NotSupported);
        }
        let (ep_in, ep_out) = iface_bulk_endpoints(cfg_desc, iface).ok_or_else(|| {
            warn!(
                "[xhci] msc slot={} iface={}: no declara una pareja de endpoints bulk",
                slot, iface
            );
            DeviceError::NotSupported
        })?;
        // Un TD de una pagina cubre el CBW, el CSW y un INQUIRY de sobra.
        let dci_in = self.configure_endpoint(
            slot,
            csz,
            ep_in.0,
            EP_TYPE_BULK_IN,
            ep_in.1,
            0,
            MAX_HID_TD,
            64,
        )?;
        let dci_out = self.configure_endpoint(
            slot,
            csz,
            ep_out.0,
            EP_TYPE_BULK_OUT,
            ep_out.1,
            0,
            MAX_HID_TD,
            64,
        )?;
        // GET_MAX_LUN es opcional: un STALL significa una sola unidad logica.
        let max_lun = self.msc_max_lun(slot, iface).unwrap_or(0);
        for m in self
            .mscs
            .iter_mut()
            .filter(|m| m.slot == slot && m.iface == iface)
        {
            if let Some(d) = m.disk.take() {
                self.pending_disk_unregs.push(d);
            }
        }
        self.mscs.retain(|m| !(m.slot == slot && m.iface == iface));
        self.mscs.push(MscDev {
            slot,
            iface,
            dci_in,
            dci_out,
            max_lun,
            inquiry: ScsiInquiry::default(),
            capacity: None,
            next_tag: 1,
            disk_id: NEXT_DISK_ID.fetch_add(1, Ordering::Relaxed),
            disk: None,
            bounce: None,
        });
        let idx = self.mscs.len() - 1;
        if subclass != MSC_SUBCLASS_SCSI {
            // RBC, MMC y UFI hablan otro conjunto de ordenes. Queda apuntada
            // para que se vea en `/proc/usbhid`, pero no se le pregunta nada.
            warn!(
                "[xhci] msc slot={} iface={}: bInterfaceSubClass={:#04x} no es SCSI \
                 transparente; no se le preguntan ordenes",
                slot, iface, subclass
            );
            return Ok(());
        }
        self.msc_identify(idx);
        Ok(())
    }

    /// `GET_MAX_LUN`: peticion de clase a la interfaz, un byte de respuesta.
    fn msc_max_lun(&mut self, slot: u8, iface: u8) -> DeviceResult<u8> {
        let buf = DmaBuf::new(64, 64)?;
        buf.flush(0, 64);
        if let Err(e) = self.ep0_control_in(
            slot,
            trb_setup(0xa1, MSC_REQ_GET_MAX_LUN, 0, iface as u16, 1, 3),
            &buf,
            1,
        ) {
            buf.leak();
            return Err(e);
        }
        buf.flush(0, 64);
        let mut raw = [0u8; 1];
        buf.read_into(0, &mut raw);
        // El campo son 4 bits: un dispositivo que devuelve basura no puede
        // reclamar dieciseis unidades logicas.
        Ok(raw[0] & 0x0f)
    }

    /// TEST UNIT READY hasta que conteste, luego INQUIRY y READ CAPACITY.
    fn msc_identify(&mut self, idx: usize) {
        // Por indice y no por referencia porque cada orden pide `&mut self`.
        // Se comprueba en cada vuelta: perder la unidad a media identificacion
        // es un desenchufe, no un panico.
        let Some(slot) = self.mscs.get(idx).map(|m| m.slot) else {
            return;
        };
        let mut ready = false;
        for attempt in 0..MSC_READY_ATTEMPTS {
            match self.msc_command(idx, &[SCSI_TEST_UNIT_READY, 0, 0, 0, 0, 0], None) {
                Ok((_, 0)) => {
                    ready = true;
                    break;
                }
                Ok((_, st)) => {
                    // La unidad contesta «no lista» mientras arranca. Pedirle
                    // el sentido es lo que limpia su condicion de atencion; sin
                    // eso, algunas repiten el mismo fallo para siempre.
                    let sense = DmaBuf::new(64, 64).ok();
                    if let Some(sb) = sense {
                        let _ = self.msc_command(
                            idx,
                            &[SCSI_REQUEST_SENSE, 0, 0, 0, 18, 0],
                            Some((&sb, 18, true)),
                        );
                    }
                    if attempt + 1 == MSC_READY_ATTEMPTS {
                        warn!(
                            "[xhci] msc slot={}: no lista tras {} intentos (CSW status {})",
                            slot, MSC_READY_ATTEMPTS, st
                        );
                    }
                    xhci_spin_delay_us(MSC_READY_WAIT_US);
                }
                Err(e) => {
                    warn!("[xhci] msc slot={}: TEST UNIT READY fallo ({:?})", slot, e);
                    return;
                }
            }
        }

        let Ok(buf) = DmaBuf::new(256, 64) else {
            return;
        };
        buf.flush(0, 256);
        match self.msc_command(idx, &[SCSI_INQUIRY, 0, 0, 0, 36, 0], Some((&buf, 36, true))) {
            Ok((_, 0)) => {
                let mut raw = [0u8; 36];
                buf.read_into(0, &mut raw);
                if let (Some(inq), Some(m)) = (scsi_parse_inquiry(&raw), self.mscs.get_mut(idx)) {
                    m.inquiry = inq;
                    let luns = m.max_lun as u16 + 1;
                    info!(
                        "[xhci] msc slot={}: {} {} {} (tipo {:#04x}, {}extraible, {} LUN)",
                        slot,
                        scsi_text(&inq.vendor),
                        scsi_text(&inq.product),
                        scsi_text(&inq.revision),
                        inq.dev_type,
                        if inq.removable { "" } else { "no " },
                        luns,
                    );
                }
            }
            other => {
                warn!(
                    "[xhci] msc slot={}: INQUIRY no contesto ({:?})",
                    slot, other
                );
                return;
            }
        }
        if !ready {
            // Sin medio dentro no hay capacidad que leer, y preguntarla solo
            // suma un fallo mas al log.
            return;
        }
        buf.flush(0, 256);
        if let Ok((_, 0)) = self.msc_command(
            idx,
            &[SCSI_READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            Some((&buf, 8, true)),
        ) {
            let mut raw = [0u8; 8];
            buf.read_into(0, &mut raw);
            let cap = scsi_parse_capacity10(&raw);
            if let Some(c) = cap {
                if c.needs_16 {
                    buf.flush(0, 256);
                    let cdb = [
                        SCSI_SERVICE_ACTION_IN_16,
                        SCSI_SAI_READ_CAPACITY_16,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        32,
                        0,
                        0,
                    ];
                    if let Ok((_, 0)) = self.msc_command(idx, &cdb, Some((&buf, 32, true))) {
                        let mut raw16 = [0u8; 32];
                        buf.read_into(0, &mut raw16);
                        let parsed = scsi_parse_capacity16(&raw16);
                        if let Some(m) = self.mscs.get_mut(idx) {
                            m.capacity = parsed;
                        }
                    }
                } else if let Some(m) = self.mscs.get_mut(idx) {
                    m.capacity = Some(c);
                }
            }
            if let Some(c) = self.mscs.get(idx).and_then(|m| m.capacity) {
                info!(
                    "[xhci] msc slot={}: {} sectores de 512 B ({} bloques de {} B)",
                    slot,
                    scsi_sectors_512(&c),
                    c.last_lba.saturating_add(1),
                    c.block_size
                );
                // Con capacidad ya es un disco. Sin ella no: un lector de
                // tarjetas vacio contesta el INQUIRY y nada mas, y darlo de
                // alta pondria en el sistema un disco de cero sectores que
                // falla cada lectura.
                if let Some(id) = self.mscs.get(idx).map(|m| m.disk_id) {
                    if !self.pending_disk_regs.contains(&id) {
                        self.pending_disk_regs.push(id);
                    }
                }
            }
        }
    }

    /// Barre los puertos de los hubs conocidos buscando enchufes y
    /// desenchufes.
    ///
    /// Un hub avisa de esos cambios por su endpoint de interrupción de cambio
    /// de estado, que este driver todavía no arma, así que se sondean los
    /// puertos: a [`HUB_SCAN_PERIOD_US`], porque cada puerto cuesta una
    /// transferencia de control.
    fn scan_hubs_if_due(&mut self) {
        if self.hubs.is_empty() {
            return;
        }
        let now = timer_now_us();
        let due: Vec<(u8, u8)> = self
            .hubs
            .iter()
            .filter(|h| {
                h.scan_due
                    || now.wrapping_sub(h.scan_last_us)
                        >= if h.ep_dci != 0 {
                            HUB_BACKSTOP_PERIOD_US
                        } else {
                            HUB_SCAN_PERIOD_US
                        }
            })
            .map(|h| (h.slot, h.ports))
            .collect();
        for (slot, ports) in due {
            if let Some(h) = self.hubs.iter_mut().find(|h| h.slot == slot) {
                h.scan_last_us = now;
                h.scan_due = false;
            }
            if self
                .slot_topo
                .get(slot as usize)
                .copied()
                .flatten()
                .is_none()
            {
                continue;
            }
            for p in 1..=ports {
                if let Err(e) = self.try_hub_port(slot, p) {
                    // Si el hub deja de contestar, insistir en los puertos que
                    // quedan solo multiplica el coste del fallo.
                    warn!(
                        "[xhci] hub slot={} puerto {}: el sondeo falló ({:?}), se deja el \
                         resto del hub para la próxima vuelta",
                        slot, p, e
                    );
                    break;
                }
            }
        }
    }

    /// Libera `slot` y todo lo que esté enchufado detrás, de lo más profundo a
    /// lo más superficial: un hub que se va se lleva su subárbol, y el
    /// controlador no puede quedarse con un Disable Slot hecho sobre un padre
    /// al que sus hijos todavía nombran como su TT.
    fn cleanup_slot_tree(&mut self, slot: u8) -> DeviceResult<()> {
        let mut order = alloc::vec![slot];
        let mut i = 0;
        while i < order.len() {
            let parent = order[i];
            for s in 1..=self.max_slots {
                if !order.contains(&s)
                    && self
                        .slot_topo
                        .get(s as usize)
                        .copied()
                        .flatten()
                        .is_some_and(|t| t.parent_slot == parent)
                {
                    order.push(s);
                }
            }
            i += 1;
        }
        for s in order.into_iter().rev() {
            self.free_slot(s)?;
        }
        Ok(())
    }

    fn setup_hid_from_config(
        &mut self,
        slot: u8,
        csz: usize,
        port: u8,
        vid: u16,
        pid: u16,
    ) -> DeviceResult<()> {
        let sniff = DmaBuf::new(64, 64)?;
        sniff.flush(0, 64); // evict stale zeros before DMA
        if let Err(e) = self.ep0_control_in(slot, trb_setup(0x80, 0x06, 0x0200, 0, 9, 3), &sniff, 9)
        {
            sniff.leak();
            return Err(e);
        }
        sniff.flush(0, 64); // invalidate so CPU reads fresh DMA data
        let mut hdr = [0u8; 9];
        sniff.read_into(0, &mut hdr);
        let total = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
        if !(9..=8192).contains(&total) {
            return Err(DeviceError::InvalidParam);
        }
        let buf_len = (total.div_ceil(64) * 64).max(64);
        let cfgb = DmaBuf::new(buf_len, 64)?;
        cfgb.flush(0, buf_len); // evict stale zeros before DMA
        if let Err(e) = self.ep0_control_in(
            slot,
            trb_setup(0x80, 0x06, 0x0200, 0, total as u16, 3),
            &cfgb,
            total as u32,
        ) {
            cfgb.leak();
            return Err(e);
        }
        cfgb.flush(0, buf_len); // invalidate so CPU reads fresh DMA data

        let mut raw = alloc::vec![0u8; total];
        cfgb.read_into(0, &mut raw[..total]);
        info!(
            "[xhci] Configuration Descriptor slot={} bytes: {:?}",
            slot, raw
        );
        // Apuntar TODA interfaz, no solo las que este driver sabe usar: un
        // dispositivo cuya única interfaz es de una clase que no manejamos es
        // exactamente el caso que había que poder ver desde fuera.
        {
            let (ifaces, dropped) = config_interfaces(&raw);
            if let Some(d) = self.devs.iter_mut().find(|d| d.slot == slot) {
                d.ifaces = ifaces;
                d.ifaces_dropped = dropped;
            }
        }

        let config_val = raw.get(5).copied().unwrap_or(1).max(1);

        // SET_CONFIGURATION
        let _ = self.ep0_control_out0(
            slot,
            trb_setup(0x00, 0x09, config_val as u16, 0, 0, 0),
            true,
        );

        // Las interfaces de almacenamiento masivo se preparan antes de recorrer
        // los endpoints HID: `setup_mass_storage` emite su propio Configure
        // Endpoint y ordenes SCSI, y entrelazarlo con el recorrido de abajo
        // dejaria el descriptor a medio leer entre comando y comando.
        for i in config_interfaces(&raw).0 {
            if i.class == USB_CLASS_MASS_STORAGE {
                if let Err(e) = self.setup_mass_storage(slot, csz, i.num, i.subclass, i.proto, &raw)
                {
                    warn!(
                        "[xhci] slot={} iface={}: almacenamiento masivo no preparado ({:?})",
                        slot, i.num, e
                    );
                }
            }
        }

        let mut o = 0usize;
        let mut cur_iface: Option<(u8, u8, u8, u16)> = None; // num, proto, subclass, report_desc_len
        while o + 2 <= total {
            let dl = raw[o] as usize;
            let dt = raw[o + 1];
            if dl < 2 || o + dl > total {
                break;
            }
            if dt == USB_DESC_IFACE && dl >= 9 {
                let iface_num = raw[o + 2];
                let iclass = raw[o + 5];
                let isub = raw[o + 6];
                let iproto = raw[o + 7];
                if iclass == USB_CLASS_HID {
                    cur_iface = Some((iface_num, iproto, isub, 0));
                } else {
                    cur_iface = None;
                }
            }
            if dt == USB_DESC_HID && dl >= 9 {
                if let Some((_, _, _, ref mut rlen)) = cur_iface {
                    // HID class descriptor: first optional descriptor is the report.
                    if raw[o + 6] == USB_DESC_HID_REPORT {
                        *rlen = u16::from_le_bytes([raw[o + 7], raw[o + 8]]);
                    }
                }
            }
            if dt == USB_DESC_EP && dl >= 7 {
                if let Some((iface, proto, subclass, rlen)) = cur_iface {
                    let addr = raw[o + 2];
                    let attr = raw[o + 3];
                    // wMaxPacketSize bits 10:0 are the size; bits 12:11 are
                    // the HS additional-transactions field and must not leak
                    // into the endpoint context's MPS field.
                    let mps = u16::from_le_bytes([raw[o + 4], raw[o + 5]]) & 0x7ff;
                    let interval = raw[o + 6];
                    if (addr & 0x80) != 0 && (attr & 3) == 3 {
                        // Interrupt IN
                        if let Err(_e) = self.init_single_hid(
                            slot, csz, port, iface, proto, subclass, rlen, addr, mps, interval,
                            vid, pid,
                        ) {}
                    }
                }
            }
            o += dl;
        }
        Ok(())
    }

    fn classify_hid_iface(
        &mut self,
        slot: u8,
        iface: u8,
        proto: u8,
        subclass: u8,
        report_desc_len: u16,
        vid: u16,
        pid: u16,
        desc_out: &mut ([u8; REPORT_DESC_SNAPSHOT], usize),
        parsed_out: &mut HidDescInfo,
    ) -> DeviceResult<u8> {
        if is_vm_abs_tablet(vid, pid, proto) {
            return Ok(HID_PROTO_TABLET);
        }
        if proto == HID_PROTO_KEY {
            return Ok(HID_PROTO_KEY);
        }
        let mouse_by_protocol =
            proto == HID_PROTO_MOUSE || (proto == 0 && subclass == HID_SUBCLASS_BOOT);
        let sniffed = if reads_report_descriptor(proto, vid, pid, report_desc_len) {
            self.sniff_hid_report(
                slot,
                iface,
                report_desc_len,
                mouse_by_protocol,
                desc_out,
                parsed_out,
            )
            .unwrap_or(HidClass::Unknown)
        } else {
            HidClass::Unknown
        };
        if mouse_by_protocol {
            return Ok(HID_PROTO_MOUSE);
        }
        // protocol 0: real hardware. Never default this to tablet — that flag
        // silences PS/2 and boot-protocol USB mice (keyboard still works).
        let role = match sniffed {
            HidClass::Key => HID_PROTO_KEY,
            HidClass::Mouse => HID_PROTO_MOUSE,
            HidClass::Tablet => HID_PROTO_TABLET,
            HidClass::Skip => 0,
            // Never default protocol-0 to tablet: that flag silences PS/2 and
            // boot-protocol USB mice (keyboard still works, pointer is dead).
            HidClass::Unknown => HID_PROTO_MOUSE,
        };
        warn!(
            "[xhci] HID slot={} iface={} bInterfaceProtocol={} subclass={} vid={:04x}:{:04x} sniffed={:?} → proto={}",
            slot, iface, proto, subclass, vid, pid, sniffed, role
        );
        if role == HID_PROTO_MOUSE
            && self
                .hids
                .iter()
                .any(|h| h.slot_id == slot && h.protocol == HID_PROTO_MOUSE)
        {
            // Extra vendor iface on a device that already has a boot mouse.
            warn!(
                "[xhci] skip extra proto-0 pointer slot={} iface={}",
                slot, iface
            );
            return Ok(0);
        }
        Ok(role)
    }

    fn sniff_hid_report(
        &mut self,
        slot: u8,
        iface: u8,
        report_desc_len: u16,
        mouse_by_protocol: bool,
        desc_out: &mut ([u8; REPORT_DESC_SNAPSHOT], usize),
        parsed_out: &mut HidDescInfo,
    ) -> Option<HidClass> {
        // `HID_MAX_DESCRIPTOR_SIZE` in Linux. A 1024-byte ceiling truncated
        // the descriptor of any keyboard with a full media/macro section, and
        // a truncated descriptor parses into a layout that does not match the
        // reports the device actually sends.
        let len = (report_desc_len as usize).clamp(1, 4096);
        let buf_len = len.div_ceil(64).max(1) * 64;
        let buf = DmaBuf::new(buf_len, 64).ok()?;
        buf.flush(0, buf_len);
        // GET_DESCRIPTOR(Report) at the interface. Optional: a stall must not
        // abort enumeration; the caller falls back to "mouse".
        if self
            .ep0_control_in(
                slot,
                trb_setup(
                    0x81,
                    0x06,
                    (USB_DESC_HID_REPORT as u16) << 8,
                    iface as u16,
                    len as u16,
                    3,
                ),
                &buf,
                len as u32,
            )
            .is_err()
        {
            buf.leak();
            return None;
        }
        buf.flush(0, buf_len);
        let mut raw = alloc::vec![0u8; len];
        buf.read_into(0, &mut raw);
        if raw.iter().all(|&b| b == 0) {
            return None;
        }
        // Stash the first bytes for /proc/usbhid: the report descriptor is what
        // tells us the report layout (report-ID prefix, field sizes) when a
        // report-protocol pointer needs parsing.
        let keep = raw.len().min(desc_out.0.len());
        desc_out.0[..keep].copy_from_slice(&raw[..keep]);
        desc_out.1 = keep;
        let class = classify_hid_report(&raw);
        *parsed_out = iface_desc_info(class, parse_hid_descriptor(&raw), mouse_by_protocol);
        Some(class)
    }

    /// Configura un endpoint del dispositivo `slot` y le deja su anillo de
    /// transferencia montado y vacío. Devuelve el DCI.
    ///
    /// Lo usan las interfaces HID, el endpoint de cambio de estado de un hub y
    /// los endpoints bulk de un dispositivo de almacenamiento: la parte
    /// delicada --copiar el Slot Context entero cuando A0=1, subir Context
    /// Entries, el intervalo y el Max ESIT Payload-- es la misma y tener
    /// varias copias de ella es como se rompe una.
    fn configure_endpoint(
        &mut self,
        slot: u8,
        csz: usize,
        ep_addr: u8,
        ep_type: u32,
        mps: u16,
        interval: u8,
        avg_trb_len: usize,
        ring_trbs: usize,
    ) -> DeviceResult<u8> {
        let dci = dci_from_ep_addr(ep_addr).ok_or(DeviceError::InvalidParam)? as usize;

        let cfg = DmaBuf::new(33 * csz, 64)?;
        // Input Control Context: add Slot (A0) and the new endpoint (A_dci)
        cfg.write_u32(4, 0x01 | (1u32 << dci));

        // Copy the current Device Slot Context (at device-context offset 0) into the Input
        // Slot Context (at input-context offset csz).  The xHCI spec requires software to
        // supply a complete, valid Slot Context whenever A0=1 in a Configure Endpoint
        // command – writing zeros would corrupt the USB device address and port fields.
        if let Some(dev) = self.dev_ctx.get(slot as usize).and_then(|o| o.as_ref()) {
            dev.flush(0, csz); // Invalidate cache lines so we read the fresh Device Context updated by the controller
            for i in 0..(csz / 4) {
                cfg.write_u32(csz + i * 4, dev.read_u32(i * 4));
            }
        }
        // Raise Context Entries to cover the new endpoint DCI.
        let slot_dw0 = cfg.read_u32(csz);
        let cur_entries = (slot_dw0 >> 27) & 0x1f;
        let new_entries = (dci as u32).max(cur_entries);
        cfg.write_u32(csz, (slot_dw0 & !(0x1f << 27)) | (new_entries << 27));

        let ep_off = csz + csz + (dci - 1) * csz;

        // Endpoint Context DW0. Interval is bits 23:16 (xHCI 1.2 table 6-9);
        // bits 31:24 are Max ESIT Payload Hi, RsvdZ unless LEC=1. Un endpoint
        // bulk no tiene intervalo: el campo es RsvdZ para él (§6.2.3.6), y
        // meterle el exponente de un endpoint de interrupción es un Parameter
        // Error en los controladores estrictos.
        let speed = self.slot_speed[slot as usize];
        let bulk = ep_type == EP_TYPE_BULK_IN || ep_type == EP_TYPE_BULK_OUT;
        cfg.write_u32(
            ep_off,
            if bulk {
                0
            } else {
                xhci_endpoint_interval(speed, interval) << 16
            },
        );

        // Endpoint Context DW1: Error Count field (bits 2:1) = 3 (value 3 << 1 = 0b110),
        // EP Type, Max Packet Size.  Error Count = 3 allows up to 3 retries after failure.
        let ep_ty = (3u32 << 1) | ep_type | ((mps as u32) << 16);
        cfg.write_u32(ep_off + 4, ep_ty);
        let ir = XferRing::new(ring_trbs)?;
        let irp = ir.ring_phys() | 1; // DCS = 1
        cfg.write_u64(ep_off + 8, irp);
        // Endpoint Context DW4: Max ESIT Payload Lo (bits 31:16) and Average
        // TRB Length (bits 15:0). DW4 exists for 32-byte contexts too (CSZ only
        // changes the stride), and an interrupt endpoint's Max ESIT Payload is
        // its max packet size; it used to be left at 0, which stricter xHCs
        // may reject with Parameter Error or use to under-reserve bandwidth.
        // Un endpoint bulk no reserva ancho de banda, así que su Max ESIT
        // Payload es 0 y solo lleva la longitud media de TRB.
        cfg.write_u32(
            ep_off + 16,
            (if bulk { 0 } else { (mps as u32) << 16 }) | ((avg_trb_len as u32) & 0xffff),
        );
        let ridx = Self::ri(slot, dci as u8);
        if let Some(old) = self.xfer_rings[ridx].replace(ir) {
            old.leak();
        }

        // Flush before the doorbell, not after: the controller reads the input
        // context by DMA the moment the command ring is rung.
        cfg.flush(0, 33 * csz);
        let p = self
            .cmd
            .push(trb_configure_endpoint(cfg.sub_phys(0), slot))?;
        self.mmio.ring_db(0, 0);
        if let Err(e) = self.wait_cmd_phys(p) {
            cfg.leak();
            return Err(e);
        }

        Ok(dci as u8)
    }

    fn init_single_hid(
        &mut self,
        slot: u8,
        csz: usize,
        port: u8,
        iface: u8,
        proto: u8,
        subclass: u8,
        report_desc_len: u16,
        ep_addr: u8,
        mps: u16,
        interval: u8,
        vid: u16,
        pid: u16,
    ) -> DeviceResult<()> {
        let mut report_desc: ([u8; REPORT_DESC_SNAPSHOT], usize) = ([0; REPORT_DESC_SNAPSHOT], 0);
        let mut parsed = HidDescInfo::default();
        let real_proto = self.classify_hid_iface(
            slot,
            iface,
            proto,
            subclass,
            report_desc_len,
            vid,
            pid,
            &mut report_desc,
            &mut parsed,
        )?;
        if real_proto == 0 {
            return Ok(());
        }

        // Size the interrupt-IN transfer (and its DMA buffers) to the
        // endpoint's wMaxPacketSize, never a hardcoded 8. A report-protocol
        // mouse can send a report larger than 8 bytes (16-bit axes, extra
        // buttons, a pan wheel); arming only 8 bytes made every such transfer
        // overrun into a Babble error, which we reset-and-retry forever — the
        // mouse endpoint delivered zero usable reports (/proc/usbhid reports=0)
        // even though it was correctly bound. Clamp to a sane window and keep a
        // floor that covers the boot layouts.
        let floor = match real_proto {
            // QEMU is 6 bytes [buttons, X16, Y16, wheel]; VirtualBox is 8 bytes
            // [buttons, dz, dw, pad, X16, Y16]. Keyboard boot report is 8.
            HID_PROTO_KEY | HID_PROTO_TABLET => 8usize,
            _ => 4usize,
        };
        // At least the largest input report the descriptor declares: a
        // report longer than wMaxPacketSize is legal (it arrives as several
        // packets closed by a short one) and, sharing the endpoint, a long
        // non-mouse report (Logitech HID++, 20 bytes) would otherwise babble.
        // The per-TRB buffer is a whole page, so up to MAX_HID_TD is safe.
        let report_len = (mps as usize)
            .max(parsed.max_report_bytes)
            .clamp(floor, MAX_HID_TD);

        // Pick the HID protocol whose report layout we are actually going to
        // decode. USB HID 1.11 §7.2.6: a boot-subclass interface supports two
        // protocols and the host "should not make any assumptions about the
        // device's state" — a BIOS that drove the keyboard or mouse leaves it
        // in Boot — so say which one we want.
        //
        // We used to ask for Boot unconditionally here, and that is what killed
        // the wheel on every real USB mouse. Almost every mouse advertises the
        // boot subclass, and the boot mouse report is defined as exactly three
        // bytes — buttons, X, Y. There is no wheel byte in it. Meanwhile we
        // read the device's *report*-protocol descriptor, found its Wheel field
        // somewhere past those three bytes, and then switched the device to a
        // protocol that never sends them: `read_signed_bits` ran off the end of
        // every report and returned zero, so the pointer and the buttons worked
        // and the wheel did nothing, forever. Linux's usbhid drives everything from the
        // report descriptor and keeps the device in report protocol; so do we.
        //
        // Boot is still the right request when the descriptor gave us nothing
        // to decode with, because then `dispatch_hid` falls back to the fixed
        // boot layout, which is only true of a device in boot protocol.
        //
        // Only boot-subclass interfaces get the request at all. A
        // report-protocol interface (subclass 0) has no boot protocol to switch
        // to, and asking anyway silenced a real composite keyboard+mouse's
        // mouse interface completely (/proc/usbhid reports=0) while the boot
        // keyboard on the same device kept working. VirtualBox's USB Tablet and
        // QEMU's usb-tablet are that shape too: absolute devices on
        // bInterfaceProtocol 0, which SET_PROTOCOL(Boot) can flip into a 3-byte
        // relative mouse while we keep parsing 6–8 byte absolute packets.
        if (real_proto == HID_PROTO_KEY || real_proto == HID_PROTO_MOUSE)
            && subclass == HID_SUBCLASS_BOOT
        {
            let want = hid_protocol_request(real_proto, &parsed);
            let _ = self.ep0_control_out0_optional(
                slot,
                trb_setup(0x21, HID_REQ_SET_PROTOCOL, want as u16, iface as u16, 0, 0),
                true,
            );
        }
        // SET_IDLE es opcional en USB HID spec: si el dispositivo no responde
        // (QEMU emula algunos dispositivos que ignoran SET_IDLE), continuar de todos modos.
        let _ = self.ep0_control_out0_optional(
            slot,
            trb_setup(0x21, HID_REQ_SET_IDLE, 0, iface as u16, 0, 0),
            true,
        );

        let dci = self.configure_endpoint(
            slot,
            csz,
            ep_addr,
            EP_TYPE_INT_IN,
            mps,
            interval,
            report_len,
            64,
        )? as usize;
        let ridx = Self::ri(slot, dci as u8);

        // Allocate one report buffer per pre-queued TRB so the controller can
        // race ahead by HID_QUEUE_DEPTH transfers without overwriting a buffer
        // we haven't dispatched yet. See HID_QUEUE_DEPTH docs for the why.
        let mut bufs = Vec::with_capacity(HID_QUEUE_DEPTH);
        for _ in 0..HID_QUEUE_DEPTH {
            let b = DmaBuf::new(report_len, 64)?;
            // Evict the zeroing `DmaBuf::new` just did before the controller
            // starts DMA-ing reports into this buffer. Every other DMA-in
            // buffer here is flushed for exactly this reason (the descriptor
            // and config-sniff buffers, and the whole event-ring segment);
            // these four were the only ones that were not, so the first
            // `clflush`-then-read in `dispatch_hid` could write the dirty
            // zeros back over the report that had just landed.
            b.flush(0, report_len);
            bufs.push(b);
        }
        {
            let ring = self
                .xfer_rings
                .get_mut(ridx)
                .and_then(|o| o.as_mut())
                .ok_or(DeviceError::NotSupported)?;
            for buf in &bufs {
                let norm = trb_normal(buf.sub_phys(0), report_len as u16, true);
                let _ = ring.push(norm)?;
            }
        }
        self.mmio.ring_db(slot, dci as u8);

        // 80ee:0021 is VirtualBox's USB Tablet. QEMU usb-tablet is typically
        // 0627:0001 and uses a different report layout (see dispatch_hid).
        let vbox_tablet = real_proto == HID_PROTO_TABLET
            && vid == VBOX_USB_TABLET_VID
            && pid == VBOX_USB_TABLET_PID;
        if vbox_tablet {
            warn!(
                "[xhci] VirtualBox USB Tablet on slot={} — abs report [btn,dz,dw,pad,X16,Y16]",
                slot
            );
        }

        self.hids.push(HidDev {
            slot_id: slot,
            port_id: port,
            ep_dci: dci as u8,
            ring_idx: ridx,
            protocol: real_proto,
            report_len,
            vbox_tablet,
            bufs,
            dispatch_idx: 0,
            enqueue_idx: 0,
            last_mods: 0,
            last_buttons: 0,
            last_keys: [0; 6],
            iface,
            if_proto: proto,
            subclass,
            vid,
            pid,
            last_report: [0; 16],
            last_report_len: 0,
            report_count: 0,
            report_desc: report_desc.0,
            report_desc_len: report_desc.1,
            mouse_layout: parsed.mouse,
            boot_reports: false,
            wheel_count: 0,
            last_wheel: (0, 0),
            // A boot-protocol keyboard has no report descriptor to parse (we
            // never read one), and its report IS the boot layout.
            key_layout: parsed.key.or(if real_proto == HID_PROTO_KEY {
                Some(BOOT_KEY_LAYOUT)
            } else {
                None
            }),
        });
        // Two ways a pointer can be bound and still deliver nothing, both
        // reported at ERROR level because that is the only level a rig booted
        // with `LOG=error` prints -- and a dead mouse is exactly when the
        // console is the only diagnostic left. /proc/usbhid has the detail;
        // these lines are what says to go look.
        if real_proto == HID_PROTO_MOUSE
            && parsed.mouse.is_empty()
            && subclass != HID_SUBCLASS_BOOT
            && proto == 0
        {
            error!(
                "[xhci] mouse slot={} vid={:04x} pid={:04x} iface={} bound but its report \
                 descriptor did not parse: NO pointer events will be delivered. \
                 Paste `cat /proc/usbhid` to get its report_desc.",
                slot, vid, pid, iface
            );
        } else if real_proto == HID_PROTO_MOUSE
            && !parsed.mouse.is_empty()
            && !parsed.mouse.has_wheel()
        {
            // ERROR, not info: a mouse whose descriptor declares no Wheel is
            // a mouse whose wheel cannot ever work, and a rig booted with
            // `LOG=error` prints nothing else. This is the line that answers
            // "the wheel does nothing" without needing /proc/usbhid.
            error!(
                "[xhci] mouse slot={} vid={:04x} pid={:04x} iface={} declares NO wheel field in \
                 its report descriptor: scrolling cannot work on this device. \
                 Paste `cat /proc/usbhid` to get its report_desc.",
                slot, vid, pid, iface
            );
        }
        if real_proto == HID_PROTO_TABLET {
            if is_vm_abs_tablet(vid, pid, proto) {
                warn!(
                    "[xhci] absolute USB pointer slot={} vid={:04x} pid={:04x} (VM tablet)",
                    slot, vid, pid
                );
            } else {
                // The tablet arm of `dispatch_hid` decodes exactly two
                // layouts, QEMU's and VirtualBox's. A real absolute device --
                // a digitizer, a touchscreen, a gamepad -- is not either of
                // them, so its reports are decoded as whichever of those two
                // it is not, and the cursor jumps. Error level because that is
                // the only level a rig booted with `LOG=error` prints, and
                // because this is the line that says which device to unplug.
                error!(
                    "[xhci] slot={} vid={:04x} pid={:04x} iface={} declares ABSOLUTE X/Y but is \
                     not a VM tablet: its reports are decoded with a VM layout and will be \
                     meaningless. Relative pointers are NOT affected.",
                    slot, vid, pid, iface
                );
            }
        }
        self.refresh_abs_pointer();
        Ok(())
    }

    /// Recompute [`USB_ABS_POINTER`] from the interfaces that are bound right
    /// now.
    ///
    /// This used to be a write-once latch set the moment any interface was
    /// classified as absolute, and it is the single switch that silences every
    /// RELATIVE pointer on the machine -- `dispatch_hid` drops USB mouse
    /// reports while it is set, and the PS/2 aux path reads it through
    /// `usb_abs_pointer_active()`. Two consequences of latching it, both of
    /// which end with a user who has no pointer at all:
    ///
    /// * Unplugging the tablet left it set with nothing absolute remaining, so
    ///   a USB mouse plugged in afterwards enumerated fine, counted up its
    ///   reports in `/proc/usbhid`, and delivered nothing.
    /// * Only a VM's absolute tablet has the double-pointer problem this
    ///   exists to solve. A real digitiser, touchscreen or gamepad also
    ///   declares absolute X/Y, and on real hardware that took the user's
    ///   mouse down with it. So only the recognised VM tablets count.
    fn refresh_abs_pointer(&self) {
        let abs = self
            .hids
            .iter()
            .any(|h| h.protocol == HID_PROTO_TABLET && is_vm_abs_tablet(h.vid, h.pid, h.if_proto));
        if USB_ABS_POINTER.swap(abs, Ordering::Relaxed) != abs {
            warn!(
                "[xhci] relative pointers (PS/2 aux and USB mice) are now {}",
                if abs {
                    "SILENCED by a VM tablet"
                } else {
                    "live"
                }
            );
        }
    }

    /// Decode and deliver the report in `bufs[dispatch_idx]`. The caller
    /// (`handle_hid_transfer_side`) owns the dispatch head and has already
    /// advanced it, so a completion drained without a listener still keeps
    /// the two ring heads in lockstep.
    fn dispatch_hid(
        &mut self,
        idx: usize,
        dispatch_idx: usize,
        actual_len: usize,
        lis: &EventListener<InputEvent>,
    ) {
        let h = match self.hids.get_mut(idx) {
            Some(h) => h,
            None => return,
        };
        // Decode only what the controller actually wrote. `tmp` is zeroed
        // beyond that, so a field that falls outside a short report reads as 0
        // instead of the stale tail of the previous report in this buffer.
        //
        // A zero-length transfer is legal on an interrupt IN endpoint and
        // means "nothing to report"; decoding it would read as every key and
        // every button released.
        if actual_len == 0 {
            return;
        }
        let report_len = h.report_len.min(actual_len);
        let buf_phys = h.bufs[dispatch_idx].phys;
        let v = phys_to_virt(buf_phys);
        // Hold the whole report: a report-protocol mouse layout can place fields
        // past byte 8 (report ID + 16-bit axes + wheel/pan).
        let mut tmp = [0u8; 64];
        let n = report_len.min(tmp.len());
        tmp[..n].fill(0);
        // Asegurar consistencia de datos en arquitecturas con caché no coherente o mapeos WB.
        // Se debe invalidar ANTES de copiar los datos para que la CPU lea de la RAM (DMA).
        #[cfg(target_arch = "x86_64")]
        {
            let mut addr = v;
            let end = v + report_len;
            while addr < end {
                unsafe {
                    _mm_clflush(addr as *const u8);
                }
                addr += 64;
            }
            unsafe {
                // CLFLUSH is ordered by MFENCE, not LFENCE (Intel SDM, CLFLUSH):
                // an LFENCE here does not guarantee the invalidate completed
                // before the loads below, which is the whole point of the flush.
                _mm_mfence();
            }
        }

        unsafe {
            core::ptr::copy_nonoverlapping(v as *const u8, tmp.as_mut_ptr(), n);
        }

        // Record the raw report for /proc/usbhid before parsing it, so a
        // real-hardware pointer that never moves can be diagnosed from a text
        // VT (its bytes reveal a report-ID prefix or a non-boot layout).
        let keep = n.min(h.last_report.len());
        h.last_report = [0; 16];
        h.last_report[..keep].copy_from_slice(&tmp[..keep]);
        h.last_report_len = report_len;
        h.report_count = h.report_count.saturating_add(1);

        // Demultiplex by Report ID before anything else. One interrupt
        // endpoint can carry a keyboard report AND a mouse report under
        // different IDs -- that is what every wireless combo receiver and
        // every keyboard with a trackpad looks like -- but an interface used
        // to be pinned to a single role, so on those devices the keyboard was
        // simply never delivered.
        let rid = tmp.first().copied();
        let key_match = h
            .key_layout
            .filter(|k| k.report_id.is_none() || k.report_id == rid);
        if let Some(kl) = key_match {
            let (mods, keys) = decode_keyboard(&tmp, &kl);
            h.last_keys = emit_keyboard_delta(lis, h.last_mods, mods, &h.last_keys, &keys);
            h.last_mods = mods;
            return;
        }

        match h.protocol {
            HID_PROTO_MOUSE | HID_PROTO_KEY if report_len >= 3 => {
                if USB_ABS_POINTER.load(Ordering::Relaxed) {
                    // A USB tablet already owns the pointer; relative HID mouse
                    // packets would fight it the same way PS/2 aux does.
                } else {
                    // Decode from the report-descriptor layout when we parsed
                    // one (report-protocol mice: report ID, 12/16-bit axes,
                    // extra buttons, a pan wheel). The fixed boot layout
                    // [buttons, dx, dy, wheel, pan] is only meaningful for an
                    // interface that actually speaks boot protocol (boot
                    // subclass, or bInterfaceProtocol = Mouse); a
                    // report-protocol interface whose descriptor we could not
                    // parse gets nothing dispatched rather than garbage
                    // (its report ID decoded as a stuck button).
                    // A device that refused SET_PROTOCOL(Report) keeps
                    // sending boot reports while we hold its report-protocol
                    // layout: the wheel byte is off the end (and a layout with
                    // a Report ID would match nothing at all), so latch onto
                    // the boot layout instead. At ERROR level because a rig
                    // booted with `LOG=error` prints nothing else, and this is
                    // the one line that says why the wheel is dead.
                    let boot_layout_ok = h.subclass == HID_SUBCLASS_BOOT || h.if_proto != 0;
                    if !h.boot_reports
                        && h.protocol == HID_PROTO_MOUSE
                        && h.mouse_layout.primary().is_some_and(|ml| {
                            mouse_report_is_truncated(&ml, report_len, boot_layout_ok)
                        })
                    {
                        h.boot_reports = true;
                        error!(
                            "[xhci] mouse slot={} {:04x}:{:04x} iface={} is still in BOOT \
                             protocol: {}-byte reports where its descriptor declares {}. \
                             Falling back to the boot layout; the wheel does not exist in \
                             boot protocol, so it will not work on this device.",
                            h.slot_id,
                            h.vid,
                            h.pid,
                            h.iface,
                            report_len,
                            h.mouse_layout
                                .primary()
                                .map(|ml| ml.report_bytes)
                                .unwrap_or(0),
                        );
                    }
                    let layout = if h.boot_reports {
                        MouseReports::default()
                    } else {
                        h.mouse_layout
                    };
                    let decoded = decode_mouse_report(
                        &layout,
                        &tmp,
                        report_len,
                        h.protocol == HID_PROTO_MOUSE && boot_layout_ok,
                    );
                    if let Some(d) = decoded {
                        if d.wheel != 0 || d.hwheel != 0 {
                            h.wheel_count = h.wheel_count.saturating_add(1);
                            h.last_wheel = (d.wheel, d.hwheel);
                        }
                        h.last_buttons = emit_mouse(lis, d, h.last_buttons);
                    }
                }
            }
            HID_PROTO_TABLET if report_len >= 6 && !(h.vbox_tablet && n < 8) => {
                // QEMU usb-tablet: [buttons, X16, Y16, wheel] (6–8 bytes).
                // VirtualBox USB Tablet: [buttons, dz, dw, pad, X16, Y16]
                // (UsbMouse.cpp USBHIDT_REPORT). X/Y are 0..=32767 in both.
                // libinput maps that abs range onto the output via EVIOCGABS.
                let btn = tmp[0];
                let vbox = h.vbox_tablet;
                let (ax, ay, wheel, hwheel) = if vbox {
                    (
                        u16::from_le_bytes([tmp[4], tmp[5]]) as i32,
                        u16::from_le_bytes([tmp[6], tmp[7]]) as i32,
                        tmp[1] as i8 as i32,
                        tmp[2] as i8 as i32,
                    )
                } else {
                    (
                        u16::from_le_bytes([tmp[1], tmp[2]]) as i32,
                        u16::from_le_bytes([tmp[3], tmp[4]]) as i32,
                        if n >= 6 { tmp[5] as i8 as i32 } else { 0 },
                        if n >= 7 { tmp[6] as i8 as i32 } else { 0 },
                    )
                };
                for (mask, code) in [
                    (1u8, BTN_LEFT),
                    (2u8, BTN_RIGHT),
                    (4u8, BTN_MIDDLE),
                    (8u8, BTN_SIDE),
                    (16u8, BTN_EXTRA),
                ] {
                    let down = (btn & mask) != 0;
                    let was = (h.last_mods & mask) != 0;
                    if down != was {
                        lis.trigger(InputEvent {
                            event_type: InputEventType::Key,
                            code,
                            value: if down { 1 } else { 0 },
                        });
                    }
                }
                h.last_mods = btn;
                lis.trigger(InputEvent {
                    event_type: InputEventType::AbsAxis,
                    code: ABS_X,
                    value: ax,
                });
                lis.trigger(InputEvent {
                    event_type: InputEventType::AbsAxis,
                    code: ABS_Y,
                    value: ay,
                });
                if wheel != 0 || hwheel != 0 {
                    h.wheel_count = h.wheel_count.saturating_add(1);
                    h.last_wheel = (wheel, hwheel);
                }
                emit_scroll(lis, wheel, hwheel);
                lis.trigger(InputEvent {
                    event_type: InputEventType::Syn,
                    code: SYN_REPORT,
                    value: 0,
                });
            }
            _ => {}
        }
    }

    /// One-shot diagnostic dump when the controller halts. HCHalted (USBSTS
    /// bit 0) is only the symptom; the *cause* is in HSE (host system / DMA
    /// error) or HCE (internal error), and in the physical addresses we hand
    /// the controller. On a >4 GiB-RAM machine those DMA structures can land
    /// above 4 GiB; if the controller is not 64-bit-capable (AC64=0) — or a
    /// pointer is truncated — that is an immediate HSE. This is exactly the
    /// kind of fault that never reproduces under QEMU (2 GiB, low addresses).
    fn dump_halt_diagnostics(&self) {
        let sts = self.mmio.read_op(4);
        let cmd = self.mmio.read_op(0);
        let hcc1 = self.mmio.read_cap(0x10);
        let ac64 = (hcc1 & 1) != 0;
        let csz = (hcc1 & (1 << 2)) != 0;

        // An all-ones USBSTS is not a real status (high bits are reserved-0):
        // the controller stopped answering MMIO (powered down / off the bus).
        // Decoding the "bits" would be meaningless noise, so say so plainly.
        if sts == u32::MAX {
            error!(
                "[xhci] HALT diag: USBSTS=0xffffffff -> controller not responding to MMIO (powered down D3 or removed); not a real halt"
            );
            return;
        }

        error!(
            "[xhci] HALT diag: USBSTS={:#010x} [HCH={} HSE={} EINT={} PCD={} SRE={} CNR={} HCE={}]",
            sts,
            sts & 1,
            (sts >> 2) & 1,
            (sts >> 3) & 1,
            (sts >> 4) & 1,
            (sts >> 10) & 1,
            (sts >> 11) & 1,
            (sts >> 12) & 1,
        );
        error!(
            "[xhci] HALT diag: USBCMD={:#010x} [RS={} INTE={} HSEE={}] AC64={} CSZ(64B ctx)={}",
            cmd,
            cmd & 1,
            (cmd >> 2) & 1,
            (cmd >> 3) & 1,
            ac64,
            csz
        );

        const FOUR_GIB: u64 = 1 << 32;
        let dcbaa = self.dcbaa.phys as u64;
        let erst = self.ev.erst_phys();
        let evseg = self.ev.seg.phys as u64;
        let crcr = self.cmd.crcr();
        let scratch = self
            .scratch_tbl
            .as_ref()
            .map(|t| t.phys as u64)
            .unwrap_or(0);
        let any_high = [dcbaa, erst, evseg, crcr, scratch]
            .iter()
            .any(|&a| a >= FOUR_GIB);
        error!(
            "[xhci] HALT diag: DCBAA={:#x} ERST={:#x} EVSEG={:#x} CRCR={:#x} SCRATCH={:#x} -> any>4GiB={}",
            dcbaa, erst, evseg, crcr, scratch, any_high
        );
        if any_high && !ac64 {
            error!("[xhci] HALT diag: *** AC64=0 (32-bit controller) with DMA >4GiB -> very likely the HSE/halt cause ***");
        }
    }

    /// Drain the event ring and deliver whatever reports it carries.
    ///
    /// `may_enumerate` is true only from the timer/io-wait `poll()` path.
    /// Enumerating a port is a multi-second job -- port reset, settle delays,
    /// a handful of EP0 control transfers each with a five-second budget --
    /// and running it from `handle_irq` did all of that with interrupts
    /// disabled: plugging a device in froze the machine for as long as it
    /// took, and a device that did not answer froze it for much longer. The
    /// port is queued in `pending_port_changes` either way; `poll()` picks it
    /// up on the next tick, which is where the deferred boot enumeration
    /// already runs.
    fn process_irq_events(&mut self, lis: Option<&EventListener<InputEvent>>, may_enumerate: bool) {
        for _ in 0..512 {
            if self.pop_ev(lis).is_none() {
                break;
            }
        }
        if may_enumerate {
            self.drain_pending_port_changes();
            self.drain_pending_hub_ports();
            self.scan_hubs_if_due();
        }
        // Recover any HID endpoint that stalled during the drain above.
        self.drain_pending_ep_resets();
    }

    /// Reset and re-arm every HID interrupt endpoint that errored during the
    /// last event drain (see `pending_ep_resets`). Runs outside `pop_ev`, so the
    /// Reset Endpoint / Set TR Dequeue commands (which spin on the event ring)
    /// don't recurse into the drain loop.
    fn drain_pending_ep_resets(&mut self) {
        let resets: Vec<(u8, u8, bool)> = core::mem::take(&mut self.pending_ep_resets);
        for (slot, dci, stalled) in resets {
            // A STALL is the device's halt, not just the host's: clear it on
            // the device first, exactly as `usb_clear_halt()` does, or the
            // endpoint stalls again on the next transaction and we loop here
            // forever.
            if stalled && self.clear_endpoint_halt(slot, dci).is_err() {
                warn!(
                    "[xhci] slot={} dci={} ClearFeature(ENDPOINT_HALT) failed; \
                     resetting the host side anyway",
                    slot, dci
                );
            }
            // Reset the halted endpoint and point its TR dequeue past the failed
            // TRB (already skipped via advance_dequeue), then re-arm one TRB and
            // ring the doorbell so interrupt transfers resume.
            if self.reset_endpoint_and_dequeue(slot, dci).is_err() {
                continue;
            }
            // The replacement TRBs went onto the ring as each failure was
            // handled, so there is nothing to re-arm here: the endpoint only
            // needs its doorbell rung to start consuming them again.
            if self
                .hids
                .iter()
                .any(|h| h.slot_id == slot && h.ep_dci == dci)
            {
                fence(Ordering::SeqCst);
                self.mmio.ring_db(slot, dci);
                warn!(
                    "[xhci] recovered HID endpoint slot={} dci={} after a transfer error",
                    slot, dci
                );
            } else if self.hubs.iter().any(|h| h.slot == slot && h.ep_dci == dci) {
                // El TRB de repuesto de un hub se empuja en
                // `handle_hub_status_event`, pero un Reset Endpoint deja la
                // campana sin tocar: sin esto, el hub se queda mudo y solo el
                // sondeo lo cubre.
                fence(Ordering::SeqCst);
                self.mmio.ring_db(slot, dci);
                warn!(
                    "[xhci] recovered hub status endpoint slot={} dci={} after a transfer error",
                    slot, dci
                );
            }
        }
    }

    /// Best-effort recovery from an HCHalted controller without a full HCRST
    /// (which would tear down enumerated devices). Clears the sticky USBSTS
    /// error bits and re-asserts Run/Stop. Returns `true` if the controller
    /// observably un-halted (USBSTS.HCH cleared); the caller can then resume
    /// normal polling. Rate-limited by a backoff window so we don't pound the
    /// MMIO when the halt is genuine.
    fn try_soft_recover(&mut self) -> bool {
        let now = timer_now_us();
        if self.halt_last_attempt_us != 0
            && now.wrapping_sub(self.halt_last_attempt_us) < HALT_RECOVERY_BACKOFF_US
        {
            return false;
        }
        self.halt_last_attempt_us = now;
        self.halt_attempts = self.halt_attempts.saturating_add(1);

        let sts = self.mmio.read_op(4);
        // PCI / D3: not recoverable from this driver.
        if sts == u32::MAX {
            return false;
        }
        // Not actually halted anymore? Treat as recovered.
        if sts & 1 == 0 {
            self.halt_attempts = 0;
            return true;
        }
        // Clear sticky / W1C bits: HSE (bit 2), EINT (3), PCD (4), SRE (10).
        self.mmio
            .write_op(4, (1 << 2) | (1 << 3) | (1 << 4) | (1 << 10));
        // Re-assert RS=1. If INTE was on before, preserve it; if MSI is wired,
        // make sure it stays enabled.
        let mut cmd = self.mmio.read_op(0);
        cmd |= 1;
        if self.msi_vector > 0 {
            cmd |= 1 << 2;
        }
        self.mmio.write_op(0, cmd);

        // Brief wait for HCH to drop. A genuinely dead controller will time
        // out here; a transient bus error usually clears within a few µs.
        let start = timer_now_us();
        let mut spins = 0u64;
        while !xhci_wait_expired(start, HALT_RECOVERY_WAIT_US, spins) {
            if self.mmio.read_op(4) & 1 == 0 {
                warn!(
                    "[xhci] HCHalted recovered after soft restart (attempt {})",
                    self.halt_attempts
                );
                self.halt_attempts = 0;
                return true;
            }
            spins = spins.saturating_add(1);
            spin_loop();
        }
        false
    }

    fn handle_port_status_change(&mut self, port_id: u8) -> DeviceResult<()> {
        let off = 0x400 + (port_id as usize - 1) * 0x10;
        let sc = self.mmio.read_op(off);
        // Acknowledge the change bits we are about to act on FIRST, and only
        // those. Enumeration below takes seconds; the old code sampled PORTSC,
        // enumerated, and then wrote every change bit back as acknowledged --
        // including the CSC of an unplug that happened while it ran. That
        // unplug was then lost for good: the slot stayed allocated, its HID
        // interfaces stayed in the list, and nothing ever rescanned CCS, so
        // the port was dead until reboot.
        let acked = sc & PORTSC_CHANGE_BITS;
        if acked != 0 {
            self.mmio.write_op(off, portsc_writeback(sc, acked));
        }
        // CEC: el puerto no pudo configurar su enlace. No hay nada que hacer
        // desde aqui --el reconocimiento de arriba lo limpia y el propio
        // controlador reintenta-- pero sin esta linea un USB3 que no llega a
        // enlazar es un puerto mudo sin una sola pista en el log.
        if (sc & (1 << 23)) != 0 {
            warn!(
                "[xhci] puerto {}: CEC, el puerto no pudo configurarse (PORTSC=0x{:08x})",
                port_id, sc
            );
        }
        if (sc & (1 << 17)) != 0 {
            let ccs = (sc & 1) != 0;
            info!("[xhci] puerto {}: CSC, CCS={}", port_id, ccs);
            if ccs {
                if let Err(e) = self.try_port_hid(port_id) {
                    warn!("[xhci] fallo al enumerar puerto {}: {:?}", port_id, e);
                }
            } else {
                self.cleanup_port(port_id)?;
            }
        }
        // Whatever the port looks like NOW is what counts. A device unplugged
        // during enumeration leaves CCS clear here (and usually a fresh CSC);
        // one plugged back in leaves CCS set with no slot behind it. Requeue
        // the port in either case rather than deciding from the stale sample.
        let now = self.mmio.read_op(off);
        let ccs_now = (now & 1) != 0;
        let has_slot = self.slot_on_root_port(port_id).is_some();
        let fails = self
            .port_enum_fails
            .get_mut(port_id as usize)
            .map(|f| {
                if ccs_now == has_slot {
                    *f = 0;
                } else {
                    *f = f.saturating_add(1);
                }
                *f
            })
            .unwrap_or(PORT_ENUM_MAX_RETRIES);
        let disagrees = ccs_now != has_slot && fails < PORT_ENUM_MAX_RETRIES;
        if (now & PORTSC_CHANGE_BITS) != 0 || disagrees {
            if !self.pending_port_changes.contains(&port_id) {
                self.pending_port_changes.push(port_id);
            }
            info!(
                "[xhci] puerto {}: estado cambiado durante la enumeracion (CCS={}, slot={}), \
                 se reexamina",
                port_id, ccs_now, has_slot
            );
        } else if ccs_now != has_slot {
            warn!(
                "[xhci] puerto {}: CCS={} sin slot tras {} intentos; se deja quieto hasta el \
                 proximo CSC",
                port_id, ccs_now, fails
            );
        }
        Ok(())
    }

    /// Libera todo lo que cuelga del puerto raíz `port_id`.
    ///
    /// No es un solo slot: si en ese puerto había un hub, sus dispositivos
    /// también se han ido, y el antiguo código solo liberaba el primer slot que
    /// encontraba. Se va de lo más profundo a lo más superficial.
    fn cleanup_port(&mut self, port_id: u8) -> DeviceResult<()> {
        let mut victims: Vec<(u8, u8)> = (1..=self.max_slots)
            .filter_map(|s| {
                self.slot_topo
                    .get(s as usize)
                    .copied()
                    .flatten()
                    .filter(|t| t.root_port == port_id)
                    .map(|t| (t.depth, s))
            })
            .collect();
        victims.sort_unstable_by_key(|&(depth, _)| core::cmp::Reverse(depth));
        for (_, slot) in victims {
            self.free_slot(slot)?;
        }
        Ok(())
    }

    /// Deshace un slot: para sus endpoints, lo saca del DCBAA, lo deshabilita y
    /// devuelve (o abandona, si el Disable Slot falló) sus contextos y anillos.
    ///
    /// No toca a los hijos: eso lo hace [`Self::cleanup_slot_tree`].
    fn free_slot(&mut self, slot: u8) -> DeviceResult<()> {
        if self
            .slot_topo
            .get(slot as usize)
            .copied()
            .flatten()
            .is_none()
        {
            return Ok(());
        }
        {
            info!("[xhci] liberando el slot {}", slot);
            // Stop every endpoint this slot still has a ring for, so the
            // controller is no longer walking those TRBs. Best effort: the
            // device is already gone, and a Stop Endpoint on an endpoint that
            // is not running answers Context State Error.
            for ep in 1..32u8 {
                let ri = Self::ri(slot, ep);
                if self.xfer_rings.get(ri).is_some_and(|o| o.is_some()) {
                    let _ = self.exec_cmd(trb_stop_endpoint(slot, ep));
                }
            }
            // Take the slot out of the DCBAA BEFORE the contexts go away. The
            // old code never did this: entry N kept pointing at a device
            // context that was then dropped, so the controller was left with a
            // live pointer into freed memory.
            self.dcbaa.write_u64(slot as usize * 8, 0);
            self.dcbaa.flush(slot as usize * 8, 8);
            // Only a successful Disable Slot guarantees the controller has let
            // go of this slot's contexts and rings. If it fails, the pages are
            // abandoned instead of handed back to the allocator.
            let disabled = self.exec_cmd(trb_disable_slot(slot)).is_ok();
            if !disabled {
                warn!(
                    "[xhci] slot={} Disable Slot failed; leaking its contexts and rings \
                     rather than handing the controller freed memory",
                    slot
                );
            }
            if let Some(dev) = self.dev_ctx[slot as usize].take() {
                if disabled {
                    drop(dev);
                } else {
                    dev.leak();
                }
            }
            self.slot_topo[slot as usize] = None;
            self.slot_speed[slot as usize] = 0;
            if let Some(pos) = self.hubs.iter().position(|h| h.slot == slot) {
                let hub = self.hubs.remove(pos);
                // El controlador ya no mira este búfer si el Disable Slot pasó;
                // si no, se abandona como el resto de la DMA del slot.
                if let Some(b) = hub.buf {
                    if disabled {
                        drop(b);
                    } else {
                        b.leak();
                    }
                }
            }
            self.pending_hub_ports.retain(|&(s, _)| s != slot);
            self.devs.retain(|d| d.slot != slot);
            // El disco se va con la unidad. Dejarlo dado de alta es peor que
            // cosmetico: este slot se reasigna a lo siguiente que enchufen, y
            // una lectura por la entrada vieja caeria en otro dispositivo.
            for m in self.mscs.iter_mut().filter(|m| m.slot == slot) {
                if let Some(d) = m.disk.take() {
                    self.pending_disk_unregs.push(d);
                }
            }
            self.mscs.retain(|m| m.slot != slot);
            for ep in 1..32 {
                let ri = Self::ri(slot, ep);
                if ri < self.xfer_rings.len() {
                    if let Some(ring) = self.xfer_rings[ri].take() {
                        if disabled {
                            drop(ring);
                        } else {
                            ring.leak();
                        }
                    }
                }
            }
            if disabled {
                self.hids.retain(|h| h.slot_id != slot);
            } else {
                for h in self.hids.iter_mut().filter(|h| h.slot_id == slot) {
                    for b in h.bufs.drain(..) {
                        b.leak();
                    }
                }
                self.hids.retain(|h| h.slot_id != slot);
            }
            // The device that was silencing every relative pointer may be the
            // one that just left.
            self.refresh_abs_pointer();
        }
        Ok(())
    }
}

fn hid_usage_to_linux(u: u8) -> Option<u16> {
    Some(match u {
        0x04 => KEY_A,
        0x05 => KEY_B,
        0x06 => KEY_C,
        0x07 => KEY_D,
        0x08 => KEY_E,
        0x09 => KEY_F,
        0x0a => KEY_G,
        0x0b => KEY_H,
        0x0c => KEY_I,
        0x0d => KEY_J,
        0x0e => KEY_K,
        0x0f => KEY_L,
        0x10 => KEY_M,
        0x11 => KEY_N,
        0x12 => KEY_O,
        0x13 => KEY_P,
        0x14 => KEY_Q,
        0x15 => KEY_R,
        0x16 => KEY_S,
        0x17 => KEY_T,
        0x18 => KEY_U,
        0x19 => KEY_V,
        0x1a => KEY_W,
        0x1b => KEY_X,
        0x1c => KEY_Y,
        0x1d => KEY_Z,
        0x1e => KEY_1,
        0x1f => KEY_2,
        0x20 => KEY_3,
        0x21 => KEY_4,
        0x22 => KEY_5,
        0x23 => KEY_6,
        0x24 => KEY_7,
        0x25 => KEY_8,
        0x26 => KEY_9,
        0x27 => KEY_0,
        0x28 => KEY_ENTER,
        0x29 => KEY_ESC,
        0x2a => KEY_BACKSPACE,
        0x2b => KEY_TAB,
        0x2c => KEY_SPACE,
        0x2d => KEY_MINUS,
        0x2e => KEY_EQUAL,
        0x2f => KEY_LEFTBRACE,
        0x30 => KEY_RIGHTBRACE,
        0x31 => KEY_BACKSLASH,
        // HID 0x32 is "Keyboard Non-US # and ~", the key that sits where
        // Backslash does on an ANSI board; `hid-input.c` maps it to
        // KEY_BACKSLASH. KEY_102ND belongs to 0x64 ("Non-US \\ and |"), the
        // extra key on an ISO board, alone. Mapping both onto 102ND made the
        // `#`/`~` key of every UK, German and Spanish keyboard type the ISO
        // key instead.
        0x32 => KEY_BACKSLASH,
        0x33 => KEY_SEMICOLON,
        0x34 => KEY_APOSTROPHE,
        0x35 => KEY_GRAVE,
        0x36 => KEY_COMMA,
        0x37 => KEY_DOT,
        0x38 => KEY_SLASH,
        0x39 => KEY_CAPSLOCK,
        0x3a => KEY_F1,
        0x3b => KEY_F2,
        0x3c => KEY_F3,
        0x3d => KEY_F4,
        0x3e => KEY_F5,
        0x3f => KEY_F6,
        0x40 => KEY_F7,
        0x41 => KEY_F8,
        0x42 => KEY_F9,
        0x43 => KEY_F10,
        0x44 => KEY_F11,
        0x45 => KEY_F12,
        0x46 => KEY_SYSRQ,
        0x47 => KEY_SCROLLLOCK,
        0x48 => KEY_PAUSE,
        0x49 => KEY_INSERT,
        0x4a => KEY_HOME,
        0x4b => KEY_PAGEUP,
        0x4c => KEY_DELETE,
        0x4d => KEY_END,
        0x4e => KEY_PAGEDOWN,
        0x4f => KEY_RIGHT,
        0x50 => KEY_LEFT,
        0x51 => KEY_DOWN,
        0x52 => KEY_UP,
        0x53 => KEY_NUMLOCK,
        0x54 => KEY_KPSLASH,
        0x55 => KEY_KPASTERISK,
        0x56 => KEY_KPMINUS,
        0x57 => KEY_KPPLUS,
        0x58 => KEY_KPENTER,
        0x59 => KEY_KP1,
        0x5a => KEY_KP2,
        0x5b => KEY_KP3,
        0x5c => KEY_KP4,
        0x5d => KEY_KP5,
        0x5e => KEY_KP6,
        0x5f => KEY_KP7,
        0x60 => KEY_KP8,
        0x61 => KEY_KP9,
        0x62 => KEY_KP0,
        0x63 => KEY_KPDOT,
        0x64 => KEY_102ND,
        0x65 => KEY_COMPOSE, // Keyboard Application ("Menu")
        0x66 => KEY_POWER,
        0x67 => KEY_KPEQUAL,
        // F13..F24: the top row of a Sun/Mac/gaming board, and what every
        // macro key on a "media" keyboard is remapped to.
        0x68 => KEY_F13,
        0x69 => KEY_F14,
        0x6a => KEY_F15,
        0x6b => KEY_F16,
        0x6c => KEY_F17,
        0x6d => KEY_F18,
        0x6e => KEY_F19,
        0x6f => KEY_F20,
        0x70 => KEY_F21,
        0x71 => KEY_F22,
        0x72 => KEY_F23,
        0x73 => KEY_F24,
        0x74 => KEY_OPEN,
        0x75 => KEY_HELP,
        0x76 => KEY_PROPS,
        0x77 => KEY_FRONT,
        0x78 => KEY_STOP,
        0x79 => KEY_AGAIN,
        0x7a => KEY_UNDO,
        0x7b => KEY_CUT,
        0x7c => KEY_COPY,
        0x7d => KEY_PASTE,
        0x7e => KEY_FIND,
        0x7f => KEY_MUTE,
        0x80 => KEY_VOLUMEUP,
        0x81 => KEY_VOLUMEDOWN,
        0x85 => KEY_KPCOMMA,
        // International 1..6 and LANG 1..4: without these a JIS keyboard
        // cannot switch input method and an ABNT2 (Brazilian) one has no
        // numpad `.` and no `/` next to the right shift.
        0x87 => KEY_RO,               // International1 (JIS `\\`/`_`, ABNT2 `/`/`?`)
        0x88 => KEY_KATAKANAHIRAGANA, // International2
        0x89 => KEY_YEN,              // International3
        0x8a => KEY_HENKAN,           // International4
        0x8b => KEY_MUHENKAN,         // International5
        0x8c => KEY_KPJPCOMMA,        // International6 (ABNT2 numpad `.`)
        0x90 => KEY_HANGEUL,          // LANG1
        0x91 => KEY_HANJA,            // LANG2
        0x92 => KEY_KATAKANA,         // LANG3
        0x93 => KEY_HIRAGANA,         // LANG4
        0xb6 => KEY_KPLEFTPAREN,
        0xb7 => KEY_KPRIGHTPAREN,
        0xe0 => KEY_LEFTCTRL,
        0xe1 => KEY_LEFTSHIFT,
        0xe2 => KEY_LEFTALT,
        0xe3 => KEY_LEFTMETA,
        0xe4 => KEY_RIGHTCTRL,
        0xe5 => KEY_RIGHTSHIFT,
        0xe6 => KEY_RIGHTALT,
        0xe7 => KEY_RIGHTMETA,
        _ => return None,
    })
}

/// HID Keyboard/Keypad page: usages 1..=3 are the error indicators
/// (`ErrorRollOver`, `POSTFail`, `ErrorUndefined`), not keys. `hid-input.c`
/// leaves them unmapped and `usbkbd.c` skips every array entry `<= 3`.
const HID_KEY_ERROR_MAX: u8 = 3;

/// Emit the key transitions between two keyboard reports and return the key
/// array to latch as "currently held".
///
/// The return value matters on a rollover report. When more keys are held than
/// the report can carry, the keyboard fills every slot with `ErrorRollOver`
/// (0x01): it is telling us it does not know what is down, not that everything
/// came up. Taking it literally released every held key and then pressed them
/// all again the moment one was let go — in a game that reads as the movement
/// keys dropping out, and under autorepeat as a burst of duplicated
/// characters. We keep the previous array instead, so the held keys stay held
/// until the keyboard can report them again.
#[must_use = "the returned array is the new `last_keys`"]
fn emit_keyboard_delta(
    lis: &EventListener<InputEvent>,
    prev_m: u8,
    new_m: u8,
    prev_k: &[u8; 6],
    new_k: &[u8; 6],
) -> [u8; 6] {
    // Modifiers are a bitmap, not an array: they are always trustworthy, and a
    // rollover must not strand a held Shift.
    for bit in 0u8..8u8 {
        let m = 1u8 << bit;
        let was = (prev_m & m) != 0;
        let now = (new_m & m) != 0;
        if was == now {
            continue;
        }
        let code = match bit {
            0 => KEY_LEFTCTRL,
            1 => KEY_LEFTSHIFT,
            2 => KEY_LEFTALT,
            3 => KEY_LEFTMETA,
            4 => KEY_RIGHTCTRL,
            5 => KEY_RIGHTSHIFT,
            6 => KEY_RIGHTALT,
            7 => KEY_RIGHTMETA,
            _ => continue,
        };
        lis.trigger(InputEvent {
            event_type: InputEventType::Key,
            code,
            value: if now { 1 } else { 0 },
        });
    }
    if new_k.iter().any(|&u| u != 0 && u <= HID_KEY_ERROR_MAX) {
        lis.trigger(InputEvent {
            event_type: InputEventType::Syn,
            code: SYN_REPORT,
            value: 0,
        });
        return *prev_k;
    }
    'p: for &u in new_k {
        if u == 0 {
            continue;
        }
        for &v in prev_k {
            if v == u {
                continue 'p;
            }
        }
        if let Some(c) = hid_usage_to_linux(u) {
            lis.trigger(InputEvent {
                event_type: InputEventType::Key,
                code: c,
                value: 1,
            });
        }
    }
    'r: for &u in prev_k {
        if u == 0 {
            continue;
        }
        for &v in new_k {
            if v == u {
                continue 'r;
            }
        }
        if let Some(c) = hid_usage_to_linux(u) {
            lis.trigger(InputEvent {
                event_type: InputEventType::Key,
                code: c,
                value: 0,
            });
        }
    }
    lis.trigger(InputEvent {
        event_type: InputEventType::Syn,
        code: SYN_REPORT,
        value: 0,
    });
    *new_k
}

/// Un disco USB dado de alta como dispositivo de bloque.
///
/// El resto del kernel cuenta en sectores de 512 (ver `BlockScheme`) y la
/// unidad direcciona en bloques suyos, que en un disco externo pueden ser de
/// 4096. La traduccion vive aqui, y es la razon de que esta capa exista en vez
/// de llamar a `msc_rw` desde fuera.
pub struct UsbDisk {
    /// El controlador, en debil: el disco lo tiene el sistema de archivos y
    /// puede sobrevivir al controlador. Un `Arc` aqui seria un ciclo.
    ctrl: Weak<XhciUsbHid>,
    /// Ver `MscDev::disk_id`. Lo que NO se guarda aqui es el slot.
    disk_id: u64,
    name: alloc::string::String,
    /// Bloques del dispositivo y su tamano, tal cual los dio READ CAPACITY.
    dev_blocks: u64,
    dev_block_size: u32,
}

impl UsbDisk {
    /// Con el controlador cogido, una tanda en bloques del dispositivo.
    fn with_inner<F, R>(&self, f: F) -> DeviceResult<R>
    where
        F: FnOnce(&mut XhciInner) -> DeviceResult<R>,
    {
        let ctrl = self.ctrl.upgrade().ok_or(DeviceError::NotReady)?;
        let mut g = ctrl.inner.lock();
        let xi = g.as_mut().ok_or(DeviceError::NotReady)?;
        f(xi)
    }

    /// El rango de bloques del dispositivo que cubre `[byte_off, byte_off+len)`,
    /// y el desplazamiento dentro del primero.
    ///
    /// Con bloques de 512 esto es la identidad; existe por los discos de 4096,
    /// donde un sector de 512 del kernel es un cuarto de bloque y pedirle a la
    /// unidad «el bloque 7» cuando el kernel dijo «el sector 7» lee 4 KiB del
    /// sitio equivocado.
    fn dev_span(&self, byte_off: u64, len: usize) -> Option<(u64, usize, usize)> {
        let bs = self.dev_block_size as u64;
        if bs == 0 || len == 0 {
            return None;
        }
        let first = byte_off / bs;
        let last = byte_off.checked_add(len as u64 - 1)? / bs;
        let nblocks = (last - first + 1) as usize;
        Some((first, nblocks, (byte_off % bs) as usize))
    }
}

impl Scheme for UsbDisk {
    fn name(&self) -> &str {
        &self.name
    }
}

impl BlockScheme for UsbDisk {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
        if buf.is_empty() || !buf.len().is_multiple_of(512) {
            return Err(DeviceError::InvalidParam);
        }
        let byte_off = (block_id as u64)
            .checked_mul(512)
            .ok_or(DeviceError::InvalidParam)?;
        let (first, nblocks, skip) = self
            .dev_span(byte_off, buf.len())
            .ok_or(DeviceError::InvalidParam)?;
        let bs = self.dev_block_size as usize;
        if skip == 0 && buf.len() == nblocks * bs {
            // El caso de siempre: 512 por bloque, o una peticion que ya cae en
            // bloques enteros. Sin copia intermedia.
            return self.with_inner(|xi| xi.msc_rw(self.disk_id, first, MscData::Read(buf)));
        }
        let mut stage = alloc::vec![0u8; nblocks * bs];
        self.with_inner(|xi| xi.msc_rw(self.disk_id, first, MscData::Read(&mut stage)))?;
        buf.copy_from_slice(&stage[skip..skip + buf.len()]);
        Ok(())
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
        if buf.is_empty() || !buf.len().is_multiple_of(512) {
            return Err(DeviceError::InvalidParam);
        }
        let byte_off = (block_id as u64)
            .checked_mul(512)
            .ok_or(DeviceError::InvalidParam)?;
        let (first, nblocks, skip) = self
            .dev_span(byte_off, buf.len())
            .ok_or(DeviceError::InvalidParam)?;
        let bs = self.dev_block_size as usize;
        if skip == 0 && buf.len() == nblocks * bs {
            return self.with_inner(|xi| xi.msc_rw(self.disk_id, first, MscData::Write(buf)));
        }
        // Bloque del dispositivo mas grande que la peticion: hay que leer,
        // modificar y escribir. Escribir el bloque entero con el resto a ceros
        // se lleva por delante los sectores vecinos, que en una tabla de
        // particiones son la tabla.
        let mut stage = alloc::vec![0u8; nblocks * bs];
        self.with_inner(|xi| xi.msc_rw(self.disk_id, first, MscData::Read(&mut stage)))?;
        stage[skip..skip + buf.len()].copy_from_slice(buf);
        self.with_inner(|xi| xi.msc_rw(self.disk_id, first, MscData::Write(&stage)))
    }

    fn flush(&self) -> DeviceResult {
        self.with_inner(|xi| xi.msc_flush(self.disk_id))
    }

    fn block_count(&self) -> usize {
        // En sectores de 512, que es lo que cuenta `block_id`.
        let bs = self.dev_block_size.max(512) as u64;
        self.dev_blocks.saturating_mul(bs / 512) as usize
    }

    fn logical_block_size(&self) -> usize {
        (self.dev_block_size as usize).max(512)
    }
}

pub struct XhciUsbHid {
    listener: EventListener<InputEvent>,
    inner: Mutex<Option<XhciInner>>,
    pub msi_vector: usize,
    halted: AtomicBool,
    /// Lock-free summaries of `inner.hids`, refreshed by whoever already holds
    /// the lock (the IRQ drain, every `poll()` tick and the boot enumeration).
    /// Per controller, not global, because each `XhciUsbHid` is its own evdev
    /// device. See `has_rel_mouse`.
    has_rel_mouse_flag: AtomicBool,
    has_tablet_flag: AtomicBool,
    /// MMIO ops/runtime bases for a lock-free IRQ ack when `try_lock` fails.
    /// Without this, an MSI that arrives while `/proc/usbhid` holds the mutex
    /// returns without clearing USBSTS.EINT / IMAN.IP, and the controller
    /// stays silent until the next lucky poll.
    ack_op_base: AtomicUsize,
    ack_rt_base: AtomicUsize,
}

/// Lista global para drenar event rings desde el timer (QEMU / IRQ perdidos).
static POLL_INSTANCES: Mutex<Vec<Arc<XhciUsbHid>>> = Mutex::new(Vec::new());

pub fn set_poll_instance(dev: Option<Arc<XhciUsbHid>>) {
    let mut instances = POLL_INSTANCES.lock();
    match dev {
        Some(dev) => {
            if !instances.iter().any(|d| Arc::ptr_eq(d, &dev)) {
                instances.push(dev);
            }
        }
        None => instances.clear(),
    }
}

/// Respaldo periódico: drena transferencias HID sin depender de MSI (alineado al driver de referencia).
///
/// Never blocks on a lock. This is a *backup* drain for lost IRQs — MSI is the
/// real delivery path and the next tick is at most a millisecond away — so a
/// skipped poll costs nothing, while waiting for a contended lock closed a
/// genuine AB-BA cycle against the graphic console:
///
///     cpu=3 at drivers/src/usb/xhci_hid.rs:3417      <- holds the shadow fb,
///     cpu=2 at drivers/src/utils/shadow_fb.rs:113       wants the xHCI lock
///     HOLDER cpu=3 at :113
///
/// The console reaches the input subsystem while holding the framebuffer lock
/// (io-wait ticks poll HID); the HID path draws the cursor, which takes the
/// framebuffer lock. Two orders, one cycle. `try_lock` here breaks it at the
/// only point where blocking buys nothing at all.
/// Publica los discos que se enchufaron y retira los que se fueron.
///
/// Se llama SIN el cerrojo del controlador cogido, y lo coge ella sola el
/// tiempo justo de vaciar las dos colas. Dar de alta un dispositivo entra en
/// las listas de `kernel-hal`, y encadenar ese cerrojo con el del controlador
/// --que ademas toca un manejador de interrupcion-- es la forma de montar un
/// ciclo. Por eso las colas existen en vez de llamar a `hotplug` desde dentro
/// de `msc_identify`.
fn drain_disk_changes(dev: &Arc<XhciUsbHid>) {
    let mut to_add: Vec<(u64, u64, u32, alloc::string::String)> = Vec::new();
    let mut to_remove: Vec<Arc<UsbDisk>> = Vec::new();
    {
        let Some(mut g) = dev.inner.try_lock() else {
            // Otro ya lo tiene. Las colas siguen ahi para la vuelta siguiente;
            // no se pierde nada por no insistir ahora.
            return;
        };
        let Some(xi) = g.as_mut() else {
            return;
        };
        to_remove.append(&mut xi.pending_disk_unregs);
        for id in core::mem::take(&mut xi.pending_disk_regs) {
            let Some(m) = xi.mscs.iter().find(|m| m.disk_id == id) else {
                continue; // se fue entre que se apunto y ahora
            };
            let Some(cap) = m.capacity else {
                continue;
            };
            let name = alloc::format!("usbdisk{}", id);
            to_add.push((id, cap.last_lba.saturating_add(1), cap.block_size, name));
        }
    }

    for d in to_remove {
        let name = d.name.clone();
        if crate::hotplug::remove(&crate::Device::Block(d)) {
            info!("[xhci] disco {} dado de baja", name);
        }
    }
    for (id, dev_blocks, dev_block_size, name) in to_add {
        let disk = Arc::new(UsbDisk {
            ctrl: Arc::downgrade(dev),
            disk_id: id,
            name: name.clone(),
            dev_blocks,
            dev_block_size,
        });
        // Se guarda la MISMA `Arc` que se da de alta: la baja es por identidad,
        // asi que una copia equivalente no serviria para retirarla.
        if !crate::hotplug::add(crate::Device::Block(disk.clone())) {
            continue;
        }
        let Some(mut g) = dev.inner.try_lock() else {
            // Dado de alta y sin poder anotarlo: hay que retirarlo, o se queda
            // en el sistema un disco que ningun desenchufe podra quitar.
            let _ = crate::hotplug::remove(&crate::Device::Block(disk));
            warn!("[xhci] {}: no se pudo anotar el disco; se retira", name);
            continue;
        };
        match g.as_mut().and_then(|xi| {
            xi.mscs
                .iter_mut()
                .find(|m| m.disk_id == id)
                .map(|m| m.disk = Some(disk.clone()))
        }) {
            Some(()) => info!(
                "[xhci] disco {} dado de alta: {} sectores de 512 B",
                name,
                disk.block_count()
            ),
            None => {
                drop(g);
                let _ = crate::hotplug::remove(&crate::Device::Block(disk));
                warn!(
                    "[xhci] {}: la unidad se fue al darla de alta; se retira",
                    name
                );
            }
        }
    }
}

pub fn poll() {
    let Some(instances) = POLL_INSTANCES.try_lock().map(|g| g.clone()) else {
        return;
    };
    for d in instances {
        // Fast path per controller: once we have latched this specific
        // controller as dead, skip its locks/MMIO forever.
        if d.halted.load(Ordering::Relaxed) {
            continue;
        }
        let Some(mut g) = d.inner.try_lock() else {
            // Busy: an MSI handler or another poll is already draining this
            // controller, or a peer holds it while wanting a lock we hold.
            continue;
        };
        if let Some(xi) = &mut *g {
            if xi.boot_enum_pending {
                xi.boot_enum_pending = false;
                info!("[xhci] deferred boot enumeration starting");
                xi.enumerate_root_hid();
            }
            xi.mmio.ack_host_interrupt();
            let sts = xi.mmio.read_op(4);
            // `0xffffffff` is not a valid USBSTS (its high bits are reserved-0):
            // an all-ones read means the controller stopped responding to MMIO,
            // i.e. it powered down (D3) or fell off the bus — typical of an
            // unused GPU USB-C / VirtualLink xHCI with nothing plugged in. That
            // is not recoverable from this driver: latch hard.
            if sts == u32::MAX {
                if !d.halted.swap(true, Ordering::Relaxed) {
                    warn!("[xhci] USBSTS=0xffffffff: el controlador no responde (apagado D3 o ausente, p.ej. un puerto USB-C/VirtualLink de GPU vacío); se detiene el sondeo");
                    xi.dump_halt_diagnostics();
                }
                continue;
            }
            if sts & 1 != 0 {
                // Genuine HCHalted. Don't give up: try a soft recovery (clear
                // sticky USBSTS errors + re-assert RS) up to a few times before
                // permanently latching. Most real-HW halts seen so far are
                // transient HSE bursts that clear with a single restart, but
                // latching forever would make input dead until reboot — exactly
                // the "TinyX kills the keyboard after a few seconds" symptom.
                let attempts = xi.halt_attempts;
                if attempts < MAX_HALT_RECOVERY_ATTEMPTS {
                    if attempts == 0 {
                        warn!(
                            "[xhci] USBSTS=HCHalted; intentando recuperación suave (attempt {}/{})",
                            attempts + 1,
                            MAX_HALT_RECOVERY_ATTEMPTS
                        );
                        xi.dump_halt_diagnostics();
                    }
                    if !xi.try_soft_recover() {
                        // `try_soft_recover` can fail transiently (backoff
                        // window still open, or one unsuccessful attempt) and
                        // the budget is tracked in `halt_attempts`. Only latch
                        // terminally once `attempts` reaches
                        // `MAX_HALT_RECOVERY_ATTEMPTS` in the branch below.
                        continue;
                    }
                    // Fall through to normal event drain.
                } else {
                    if !d.halted.swap(true, Ordering::Relaxed) {
                        warn!(
                            "[xhci] HCHalted no se recupera tras {} intentos; se detiene el sondeo",
                            MAX_HALT_RECOVERY_ATTEMPTS
                        );
                        xi.dump_halt_diagnostics();
                    }
                    continue;
                }
            } else if xi.halt_attempts != 0 {
                // Controller is healthy again — clear the soft-recovery
                // budget so the next halt (if any) gets its own fresh shot.
                xi.halt_attempts = 0;
            }
            xi.process_irq_events(Some(&d.listener), true);
            d.refresh_role_flags(xi);
        }
        // Fuera del cerrojo, a proposito: ver `drain_disk_changes`.
        drop(g);
        drain_disk_changes(&d);
    }
}

impl XhciUsbHid {
    /// Clear USBSTS.EINT and IMAN.IP without taking `inner`. Safe for the MSI
    /// path when another CPU already holds the mutex: these two registers are
    /// write-1-to-clear / RMW of sticky interrupt bits, and a missed drain is
    /// recovered by `poll()`.
    fn ack_host_interrupt_unlocked(&self) {
        let op = self.ack_op_base.load(Ordering::Relaxed);
        let rt = self.ack_rt_base.load(Ordering::Relaxed);
        if op == 0 || rt == 0 {
            return;
        }
        fence(Ordering::Acquire);
        let usbsts = unsafe { read_volatile((op + 0x04) as *const u32) };
        if (usbsts & 0x08) != 0 {
            fence(Ordering::Release);
            unsafe { write_volatile((op + 0x04) as *mut u32, 0x08) };
            fence(Ordering::Release);
        }
        fence(Ordering::Acquire);
        let iman = unsafe { read_volatile((rt + 0x20) as *const u32) };
        fence(Ordering::Release);
        unsafe { write_volatile((rt + 0x20) as *mut u32, (iman & 0x02) | 0x01) };
        fence(Ordering::Release);
    }

    pub fn probe(
        dev: &PCIDevice,
        mmio_vaddr: usize,
        bar_size: usize,
        msi_vector: usize,
    ) -> DeviceResult<Arc<Self>> {
        let _ = dev;
        let mmio = XhciMmio::from_virt(mmio_vaddr, bar_size)?;
        let hcsp = mmio.read_cap(4);
        let max_slots = (hcsp & 0xff) as u8;
        if max_slots == 0 {
            return Err(DeviceError::InvalidParam);
        }
        let max_ports = ((hcsp >> 24) & 0xff) as u8;
        let mut inner = XhciInner::new(mmio, max_slots, max_ports, msi_vector)?;
        inner.reset_and_run()?;
        // `make qemu` feeds labwc through QEMU's `usb-kbd` + `usb-tablet` on
        // this controller. Those userspace stacks probe `/dev/input/event*`
        // exactly once during startup and expect the interrupt endpoints to be
        // armed already; leaving enumeration deferred to a later background poll
        // can make the compositor come up with permanently dead input. Enumerate
        // once here so boot reaches userspace with working devices, while the
        // periodic/IRQ poll path below still handles the steady-state event
        // draining and any later recovery.
        inner.enumerate_root_hid();
        inner.boot_enum_pending = false;
        let has_rel_mouse_flag =
            AtomicBool::new(inner.hids.iter().any(|h| h.protocol == HID_PROTO_MOUSE));
        let has_tablet_flag =
            AtomicBool::new(inner.hids.iter().any(|h| h.protocol == HID_PROTO_TABLET));
        let ack_op = inner.mmio.op_base;
        let ack_rt = inner.mmio.rt_base;
        let arc = Arc::new(Self {
            listener: EventListener::new(),
            inner: Mutex::new(Some(inner)),
            msi_vector,
            halted: AtomicBool::new(false),
            has_rel_mouse_flag,
            has_tablet_flag,
            ack_op_base: AtomicUsize::new(ack_op),
            ack_rt_base: AtomicUsize::new(ack_rt),
        });
        set_poll_instance(Some(arc.clone()));
        // La enumeracion de arranba de arriba corrio antes de que existiera
        // esta `Arc`, asi que un pendrive que ya estuviera enchufado al
        // encender esta apuntado y sin publicar. Publicarlo aqui es lo que
        // hace que un arranque con el disco puesto se vea igual que
        // enchufarlo despues.
        drain_disk_changes(&arc);
        Ok(arc)
    }

    /// Both of these are read by `capability()` and `abs_info()` from thread
    /// context, and `capability()` is on the path the console takes while it
    /// holds the framebuffer lock. Taking the xHCI lock there is what put this
    /// driver in a lock cycle with `shadow_fb` (see the note on `poll()`), and
    /// what the interrupt handler could then spin against. They are plain
    /// summaries of `hids`, so they live in atomics that the bind and unbind
    /// paths refresh, and reading them takes no lock at all.
    fn has_rel_mouse(&self) -> bool {
        self.has_rel_mouse_flag.load(Ordering::Relaxed)
    }

    fn has_tablet(&self) -> bool {
        self.has_tablet_flag.load(Ordering::Relaxed)
    }

    /// Republish the two summaries from a state the caller already holds.
    fn refresh_role_flags(&self, xi: &XhciInner) {
        self.has_rel_mouse_flag.store(
            xi.hids.iter().any(|h| h.protocol == HID_PROTO_MOUSE),
            Ordering::Relaxed,
        );
        self.has_tablet_flag.store(
            xi.hids.iter().any(|h| h.protocol == HID_PROTO_TABLET),
            Ordering::Relaxed,
        );
    }
}

impl_event_scheme!(XhciUsbHid, InputEvent);

impl Scheme for XhciUsbHid {
    fn name(&self) -> &str {
        "xhci-usb-hid"
    }

    fn handle_irq(&self, vector: usize) {
        if vector != self.msi_vector {
            return;
        }
        // `try_lock`, never `lock`. This runs in interrupt context: if a
        // thread on this same CPU is inside `debug_report()` (a `cat
        // /proc/usbhid`) or any other holder when the MSI arrives, a blocking
        // acquire here spins forever against a holder that cannot run until we
        // return. When the lock is busy, still ACK the MSI: an edge-triggered
        // interrupt is not re-delivered until USBSTS.EINT / IMAN.IP are
        // cleared, and poll() alone may not run soon enough to reopen the
        // device. The ring drain waits for the next poll tick.
        let Some(mut g) = self.inner.try_lock() else {
            self.ack_host_interrupt_unlocked();
            return;
        };
        if let Some(ref mut xi) = *g {
            xi.mmio.ack_host_interrupt();
            // No enumeration from interrupt context -- see `process_irq_events`.
            xi.process_irq_events(Some(&self.listener), false);
            self.refresh_role_flags(xi);
        }
    }
}

/// Build one `EVIOCGBIT` bitmap for this scheme's evdev node.
///
/// Split out of `capability()` so every combination of the three inputs is
/// testable: the node is one device that can carry a keyboard, a relative
/// mouse and an absolute tablet at once, and the combinations that bite are
/// exactly the ones no VM ever produces.
///
/// * `vm_tablet` — a VM absolute tablet currently owns the pointer
///   ([`USB_ABS_POINTER`]). This, and ONLY this, suppresses the relative axes.
///   It used to be `tablet` below, which is true for *any* HID interface whose
///   report descriptor declares absolute X and Y. In a VM that is only ever the
///   emulated tablet, but on real hardware a gamepad, a digitizer, a
///   touchscreen or a mouse's own vendor interface declares absolute axes too
///   — and any one of them then took `EV_REL` off the node for the whole
///   session. The keyboard kept working and the pointer was dead, in X11 and
///   in Wayland alike, because both read these bitmaps and neither will
///   deliver an event of a type the device does not declare. `USB_ABS_POINTER`
///   was narrowed to real VM tablets (`is_vm_abs_tablet`) precisely because
///   the same over-broad test silenced every relative pointer; this is the
///   place that was left behind.
/// * `rel_mouse` — a relative mouse is bound right now.
/// * `tablet` — an absolute interface is bound, so the node really does
///   deliver `ABS_X`/`ABS_Y`.
fn hid_capability(
    cap_type: CapabilityType,
    vm_tablet: bool,
    rel_mouse: bool,
    tablet: bool,
) -> InputCapability {
    let mut cap = InputCapability::empty();
    // Advertise a relative pointer whenever no VM tablet owns the pointer,
    // even before a mouse has finished USB enumeration. libinput reads these
    // capabilities ONCE, when it opens the evdev node, and on real hardware
    // the compositor can open it before the mouse enumerates.
    let want_rel = !vm_tablet || rel_mouse;
    // Every REL code advertised below, the wheel included. A tablet reports a
    // wheel of its own (`dispatch_hid`'s tablet arm calls `emit_scroll`), so
    // the wheel axes are live even when the pointer axes are not.
    let want_wheel = want_rel || tablet;
    match cap_type {
        CapabilityType::Event => {
            cap.set_all(&[EV_SYN, EV_KEY]);
            // EV_REL must be here whenever ANY REL code is advertised below.
            // It used to be set only for `want_rel`, while the wheel axes were
            // set for `want_rel || tablet`: a tablet-only node therefore
            // listed REL_WHEEL in a bitmap nothing would ever read, because
            // libevdev only asks for the REL codes of a device whose type
            // bitmap claims EV_REL. Every wheel event the tablet arm emitted
            // was then dropped before it reached the compositor -- which is
            // the entire "the mouse wheel does not work", in the one
            // configuration every QEMU run uses (`-device usb-tablet`).
            if want_wheel {
                cap.set(EV_REL);
            }
            if tablet {
                cap.set(EV_ABS);
            }
        }
        CapabilityType::Key => {
            // Advertise the full keyboard keycode block plus the mouse
            // buttons this HID scheme can emit. libinput builds the
            // device's key set from EVIOCGBIT(EV_KEY) and DROPS any key
            // event whose code is not in this bitmap.
            for code in KEY_ESC..=KEY_MICMUTE {
                cap.set(code);
            }
            cap.set_all(&[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA]);
        }
        CapabilityType::RelAxis => {
            if want_rel {
                cap.set_all(&[REL_X, REL_Y]);
            }
            // The hi-res axes go in alongside the low-res ones because
            // `emit_scroll` emits both. Advertising one without the other
            // is the one combination that breaks scrolling outright:
            // libinput only synthesises hi-res events for a device that
            // does NOT claim the axis, so a claimed-but-silent
            // REL_WHEEL_HI_RES would leave its wheel state machine with
            // nothing to integrate.
            if want_wheel {
                cap.set_all(&[REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES]);
            }
        }
        CapabilityType::AbsAxis if tablet => cap.set_all(&[ABS_X, ABS_Y]),
        CapabilityType::InputProp if tablet => cap.set(INPUT_PROP_POINTER),
        _ => {}
    }
    cap
}

impl InputScheme for XhciUsbHid {
    fn capability(&self, cap_type: CapabilityType) -> InputCapability {
        hid_capability(
            cap_type,
            // Only a VM tablet takes the relative axes away -- NOT any
            // interface that happens to declare absolute X/Y. See
            // `hid_capability`.
            USB_ABS_POINTER.load(Ordering::Relaxed),
            self.has_rel_mouse(),
            self.has_tablet(),
        )
    }

    fn abs_info(&self, axis: u16) -> Option<AbsInfo> {
        if self.has_tablet() && (axis == ABS_X || axis == ABS_Y) {
            Some(AbsInfo::range(0, TABLET_RANGE))
        } else {
            None
        }
    }

    fn debug_report(&self) -> alloc::string::String {
        use core::fmt::Write as _;
        let mut s = alloc::string::String::new();
        let abs = USB_ABS_POINTER.load(Ordering::Relaxed);
        let _ = writeln!(
            s,
            "[usbhid] abs_pointer={} has_rel_mouse={} has_tablet={}",
            abs,
            self.has_rel_mouse(),
            self.has_tablet()
        );
        // `try_lock`: /proc/usbhid is a diagnostic, and blocking here is how a
        // `cat` ends up holding the controller lock while an interrupt waits
        // on it.
        let Some(guard) = self.inner.try_lock() else {
            let _ = writeln!(s, "[usbhid] controller busy (draining events)");
            return s;
        };
        let Some(xi) = guard.as_ref() else {
            let _ = writeln!(s, "[usbhid] controller not initialised");
            return s;
        };
        if xi.devs.is_empty() {
            let _ = writeln!(s, "[usbhid] no USB devices enumerated");
        }
        for d in xi.devs.iter() {
            // Un dispositivo sin driver sale igual que uno con driver: a la
            // pregunta de si el sistema lo ve, esta linea es la respuesta.
            let _ = write!(
                s,
                "[usbhid] dev slot={} {:04x}:{:04x} class={:#04x}/{:#04x}/{:#04x} ({}) \
                 speed={} root_port={} route={:#x} tier={} ifaces=[",
                d.slot,
                d.vid,
                d.pid,
                d.class,
                d.subclass,
                d.proto,
                usb_class_name(d.class),
                d.speed,
                d.topo.root_port,
                d.topo.route,
                d.topo.depth,
            );
            for (n, i) in d.ifaces.iter().enumerate() {
                let _ = write!(
                    s,
                    "{}{}:{:#04x}/{:#04x}/{:#04x}({})",
                    if n == 0 { "" } else { " " },
                    i.num,
                    i.class,
                    i.subclass,
                    i.proto,
                    usb_class_name(i.class),
                );
            }
            if d.ifaces_dropped != 0 {
                let _ = write!(s, " +{} mas", d.ifaces_dropped);
            }
            let _ = writeln!(
                s,
                "] bound={}",
                xi.hids.iter().filter(|h| h.slot_id == d.slot).count()
            );
        }
        for m in xi.mscs.iter() {
            // La linea que contesta «si, el sistema ve tu pendrive», con su
            // nombre y su tamano. `cap=none` es una unidad que hablo pero no
            // tiene medio dentro, que es distinto de no estar.
            let _ = write!(
                s,
                "[usbhid] msc slot={} iface={} bulk_in={} bulk_out={} luns={} \
                 \"{} {} {}\" type={:#04x} removable={} ",
                m.slot,
                m.iface,
                m.dci_in,
                m.dci_out,
                m.max_lun as u16 + 1,
                scsi_text(&m.inquiry.vendor),
                scsi_text(&m.inquiry.product),
                scsi_text(&m.inquiry.revision),
                m.inquiry.dev_type,
                m.inquiry.removable,
            );
            match m.capacity {
                Some(c) => {
                    let _ = write!(
                        s,
                        "sectors512={} blocks={} block_size={} ",
                        scsi_sectors_512(&c),
                        c.last_lba.saturating_add(1),
                        c.block_size
                    );
                }
                None => {
                    let _ = write!(s, "cap=none ");
                }
            }
            // Lo PRIMERO que hay que mirar cuando una unidad sale aqui y en
            // `/dev` no hay nada: `disk=-` es que el alta no se hizo (sin
            // capacidad, o sin destino de hotplug instalado todavia), y
            // `disk=<nombre>` es que el dispositivo de bloque existe y el
            // problema esta mas arriba, en quien monta.
            match m.disk.as_ref() {
                Some(d) => {
                    let _ = writeln!(s, "disk={}", d.name());
                }
                None => {
                    let _ = writeln!(s, "disk=-");
                }
            }
        }
        for h in xi.hubs.iter() {
            let topo = xi
                .slot_topo
                .get(h.slot as usize)
                .copied()
                .flatten()
                .unwrap_or_default();
            // Sin esta linea, un teclado detras de un hub que no enumera no se
            // distingue de un hub que nunca se configuro.
            let _ = writeln!(
                s,
                "[usbhid] hub slot={} root_port={} route={:#x} tier={} ports={} speed={} \
                 status_ep={} children={}",
                h.slot,
                topo.root_port,
                topo.route,
                topo.depth,
                h.ports,
                h.speed,
                // dci=0 es «no se pudo armar»: ese hub va solo con el sondeo, y
                // es lo primero que hay que saber si un cambio no se nota.
                h.ep_dci,
                (1..=xi.max_slots)
                    .filter(|&c| xi
                        .slot_topo
                        .get(c as usize)
                        .copied()
                        .flatten()
                        .is_some_and(|t| t.parent_slot == h.slot))
                    .count()
            );
        }
        if xi.hids.is_empty() {
            let _ = writeln!(s, "[usbhid] no HID interfaces bound");
        }
        for h in xi.hids.iter() {
            let role = match h.protocol {
                HID_PROTO_KEY => "key",
                HID_PROTO_MOUSE => "mouse(rel)",
                HID_PROTO_TABLET => "tablet(abs)",
                _ => "none",
            };
            let n = h.last_report_len.min(h.last_report.len());
            let _ = write!(
                s,
                "[usbhid] slot={} iface={} bInterfaceProtocol={} subclass={} {:04x}:{:04x} \
                 role={} report_len={} reports={} wheel_events={} last_wheel={:?} last=[",
                h.slot_id,
                h.iface,
                h.if_proto,
                h.subclass,
                h.vid,
                h.pid,
                role,
                h.report_len,
                h.report_count,
                h.wheel_count,
                h.last_wheel,
            );
            for (i, b) in h.last_report[..n].iter().enumerate() {
                let _ = write!(s, "{}{:02x}", if i == 0 { "" } else { " " }, b);
            }
            let _ = writeln!(s, "]");
            for ml in h.mouse_layout.iter() {
                let bf = |f: BitField| alloc::format!("bit{}:{}", f.off, f.len);
                let _ = writeln!(
                    s,
                    "[usbhid]   layout report_id={:?} bytes={} buttons={:?} x={:?} y={:?} wheel={:?} hwheel={:?}",
                    ml.report_id,
                    ml.report_bytes,
                    ml.buttons.map(bf),
                    ml.x.map(bf),
                    ml.y.map(bf),
                    ml.wheel.map(bf),
                    ml.hwheel.map(bf),
                );
            }
            if h.boot_reports {
                let _ = writeln!(
                    s,
                    "[usbhid]   BOOT protocol reports (shorter than the descriptor declares): \
                     decoded with the fixed boot layout, no wheel"
                );
            }
            if h.mouse_layout.is_empty()
                && h.protocol == HID_PROTO_MOUSE
                && h.subclass != HID_SUBCLASS_BOOT
                && h.if_proto == 0
            {
                let _ = writeln!(
                    s,
                    "[usbhid]   layout=UNPARSED (report-protocol interface; reports are NOT dispatched — paste report_desc)"
                );
            }
            let dn = h.report_desc_len.min(h.report_desc.len());
            if dn > 0 {
                let _ = write!(s, "[usbhid]   report_desc=[");
                for (i, b) in h.report_desc[..dn].iter().enumerate() {
                    let _ = write!(s, "{}{:02x}", if i == 0 { "" } else { " " }, b);
                }
                let _ = writeln!(s, "]");
            }
        }
        s
    }
}

pub struct XhciDriverPci;

impl PciDriver for XhciDriverPci {
    fn name(&self) -> &str {
        "xhci"
    }

    fn matched(&self, vendor_id: u16, _device_id: u16) -> bool {
        // We match by class/subclass/prog_if in matched_dev instead.
        // But for simplicity, we can just return false here and use a custom logic in pci_drivers.
        // Actually, PciDriver trait should be flexible enough.
        // I'll add a matched_dev method to PciDriver if needed, but for now I'll just match common xHCI IDs or all USB controllers.
        vendor_id != 0xffff // temporary: we'll check class in init or matched
    }

    fn matched_dev(&self, dev: &PCIDevice) -> bool {
        dev.id.class == 0x0c && dev.id.subclass == 0x03
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        let (addr, len) = if let Some(BAR::Memory(ba, bl, _, _)) = dev.bars[0] {
            (ba, bl as u64)
        } else {
            return Err(DeviceError::NotSupported);
        };

        if addr == 0 {
            return Err(DeviceError::NotSupported);
        }

        let base_addr = (addr as usize) & !0xfff;
        let offset = (addr as usize) & 0xfff;
        let map_len =
            ((len.min(usize::MAX as u64) as usize + offset + 0xfff) & !0xfff).max(128 * 1024);

        // Through the base the mapper returns, with the BAR's own low bits as
        // the offset: see `bus::resolve_window`.
        let vaddr = crate::bus::resolve_window(mapper, base_addr, map_len, offset);

        let vector = irq.map(|idx| idx + 32).unwrap_or(NO_MSI_VECTOR);

        // Handle xHCI
        if dev.id.prog_if == 0x30 {
            let input = XhciUsbHid::probe(dev, vaddr, map_len, vector)?;
            if vector != NO_MSI_VECTOR {
                pci_note_pending_msi(vector, input.clone());
            }
            Ok(Device::Input(input))
        } else {
            // Legacy USB
            #[cfg(feature = "legacy-usb-hid")]
            {
                let kind = match dev.id.prog_if {
                    0x20 => LegacyUsbKind::Ehci,
                    0x10 => LegacyUsbKind::Ohci,
                    0x00 => LegacyUsbKind::Uhci,
                    _ => return Err(DeviceError::NotSupported),
                };
                let input = LegacyUsbHid::probe(kind, dev, vaddr, map_len, vector)?;
                Ok(Device::Input(input))
            }
            #[cfg(not(feature = "legacy-usb-hid"))]
            Err(DeviceError::NotSupported)
        }
    }
}

#[cfg(test)]
mod tests;

/// The MSI queue of this module, which is a **second** copy of the one in
/// `crate::net`: its own statics, its own cap, its own walk. They are wired up
/// separately by `kernel-hal` (one `pci_set_irq_host` each), so both are live,
/// and the two have already drifted apart once -- this walk used to lose the
/// devices queued behind a refusal while the other logged and carried on. These
/// tests ask this copy the same questions the `net` ones ask its twin, which is
/// the only way a divergence shows up as a red test rather than as a mouse that
/// stops working.
#[cfg(test)]
mod msi_tests;

#[cfg(test)]
mod portsc_tests;

#[cfg(test)]
mod ring_tests;

#[cfg(test)]
mod mmio_tests;

#[cfg(test)]
mod hub_tests;

#[cfg(test)]
mod disk_tests;
