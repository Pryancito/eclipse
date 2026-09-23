use crate::prelude::IrqHandler;
use crate::scheme::{IrqScheme, Scheme};
use crate::sync::Mutex;
use crate::utils::IrqManager;
use crate::DeviceResult;

pub static GICC_SIZE: usize = 0x1000;
pub static GICD_SIZE: usize = 0x1000;
static GICD_CTLR: u32 = 0x000;
static GICD_TYPER: u32 = 0x004;
static GICD_ISENABLER: u32 = 0x100;
static GICD_ICENABLER: u32 = 0x180;
static GICD_IPRIORITY: u32 = 0x400;
static GICD_ITARGETSR: u32 = 0x800;
static GICD_ICFGR: u32 = 0xc00;
static GICC_IAR: u32 = 0x000c;
static GICC_EOIR: u32 = 0x0010;
static GICC_CTLR: u32 = 0x0000;
static GICC_PMR: u32 = 0x0004;

/// Interrupt IDs a GICv2 distributor can route. 1020..=1023 are reserved
/// (1023 is "spurious"), and `pending_irq` already turns everything from
/// 0x3fe up into `usize::MAX`.
const GIC_IRQ_COUNT: usize = 1024;
const GIC_IRQ_RANGE: core::ops::Range<usize> = 0..1020;

pub struct IntController {
    gicc: GicCpuIf,
    gicd: GicDistIf,
    manager: Mutex<IrqManager<GIC_IRQ_COUNT>>,
}

struct GicDistIf {
    pub address: usize,
    pub ncpus: u32,
    pub nirqs: u32,
}

struct GicCpuIf {
    address: usize,
}

impl IntController {
    pub fn new(gicc_base: usize, gicd_base: usize) -> Self {
        Self {
            gicc: GicCpuIf { address: gicc_base },
            gicd: GicDistIf {
                address: gicd_base,
                ncpus: 0,
                nirqs: 0,
            },
            // Was 0..50, which is neither the distributor's range nor
            // anything else: SPIs start at ID 32, so a board with more than
            // eighteen of them had devices whose IRQ could not be registered
            // at all, while `is_valid_irq` below went on saying every ID was
            // fine. riscv's PLIC already sizes its table to the controller.
            manager: Mutex::new(IrqManager::new(GIC_IRQ_RANGE)),
        }
    }

    fn init(&mut self) {
        unsafe {
            // Disable IRQ Distribution
            self.gicd.write(GICD_CTLR, 0);

            let typer = self.gicd.read(GICD_TYPER);
            self.gicd.ncpus = ((typer & (0x7 << 5)) >> 5) + 1;
            self.gicd.nirqs = ((typer & 0x1f) + 1) * 32;

            // Set all SPIs to level triggered
            for irq in (32..self.gicd.nirqs).step_by(16) {
                self.gicd.write(GICD_ICFGR + ((irq / 16) * 4), 0);
            }

            // Disable all SPIs
            for irq in (32..self.gicd.nirqs).step_by(32) {
                self.gicd
                    .write(GICD_ICENABLER + ((irq / 32) * 4), 0xffff_ffff);
            }

            // Affine all SPIs to CPU0 and set priorities for all IRQs
            for irq in 0..self.gicd.nirqs {
                if irq > 31 {
                    let ext_offset = GICD_ITARGETSR + (4 * (irq / 4));
                    let int_offset = irq % 4;
                    let mut val = self.gicd.read(ext_offset);
                    val |= 0b0000_0001 << (8 * int_offset);
                    self.gicd.write(ext_offset, val);
                }

                let ext_offset = GICD_IPRIORITY + (4 * (irq / 4));
                let int_offset = irq % 4;
                let mut val = self.gicd.read(ext_offset);
                val |= 0b0000_0000 << (8 * int_offset);
                self.gicd.write(ext_offset, val);
            }

            // Enable CPU0's GIC interface
            self.gicc.write(GICC_CTLR, 1);

            // Set CPU0's Interrupt Priority Mask
            self.gicc.write(GICC_PMR, 0xff);

            // Enable IRQ distribution
            self.gicd.write(GICD_CTLR, 0x1);
        }
    }

    pub fn irq_enable(&self, irq: u32) {
        unsafe {
            let offset = GICD_ISENABLER + (4 * (irq / 32));
            self.gicd.write(offset, 1 << (irq % 32));
        }
    }

    pub fn irq_disable(&self, irq: u32) {
        unsafe {
            let offset = GICD_ICENABLER + (4 * (irq / 32));
            self.gicd.write(offset, 1 << (irq % 32));
        }
    }

    pub fn irq_eoi(&self, irq: u32) {
        unsafe {
            self.gicc.write(GICC_EOIR, irq);
        }
    }

    /// The acknowledged interrupt, as the raw GICC_IAR word, or `usize::MAX`
    /// when there is none.
    ///
    /// The whole word, not the interrupt id: for a **software-generated
    /// interrupt** (an SGI, ids 0..=15) bits [12:10] carry the id of the CPU
    /// that sent it, and GICv2 requires the same word to come back to GICC_EOIR
    /// -- an EOI that does not match the active interrupt is UNPREDICTABLE, and
    /// on this part leaves it active, so the CPU's running priority never drops
    /// and it takes no further interrupt of that priority or below. Ever.
    ///
    /// The spuriousness test therefore has to look at the id alone. Comparing
    /// the whole word against 0x3fe called every SGI from a CPU other than 0
    /// spurious -- IAR is `0x400` for CPU 1 -- and the caller then wrote
    /// `0xffff_ffff` to EOIR. SPIs are unaffected either way: their CPUID
    /// field is zero, so the word *is* the id.
    pub fn pending_irq(&self) -> usize {
        let iar = unsafe { self.gicc.read(GICC_IAR) as usize };
        if iar & 0x3ff >= 0x3fe {
            usize::MAX
        } else {
            iar
        }
    }
}

impl Scheme for IntController {
    fn name(&self) -> &str {
        "ARM Generic Interrupt Controller"
    }

    fn handle_irq(&self, irq_num: usize) {
        if irq_num != usize::MAX {
            // Dispatch on the interrupt id, acknowledge with the whole word:
            // see `pending_irq`. For everything but an SGI the two are equal,
            // so a caller passing a bare id (every caller but the aarch64 trap
            // entry) is unaffected.
            self.manager.lock().handle(irq_num & 0x3ff).ok();
        }
        self.irq_eoi(irq_num as u32);
    }
}

impl IrqScheme for IntController {
    fn is_valid_irq(&self, irq_num: usize) -> bool {
        irq_num != usize::MAX
    }

    fn mask(&self, irq_num: usize) -> DeviceResult {
        self.irq_disable(irq_num as u32);
        Ok(())
    }

    fn unmask(&self, irq_num: usize) -> DeviceResult {
        self.irq_enable(irq_num as u32);
        Ok(())
    }

    fn register_handler(&self, irq_num: usize, handler: IrqHandler) -> DeviceResult {
        self.manager
            .lock()
            .register_handler(irq_num, handler)
            .map_err(|irq_num| {
                trace!("Unknown irq_num: {:?}", irq_num);
            })
            .ok();
        Ok(())
    }

    fn unregister(&self, _irq_num: usize) -> DeviceResult {
        todo!()
    }
}

impl GicDistIf {
    unsafe fn read(&self, reg: u32) -> u32 {
        core::ptr::read_volatile((self.address + reg as usize) as *const u32)
    }

    unsafe fn write(&self, reg: u32, value: u32) {
        core::ptr::write_volatile((self.address + reg as usize) as *mut u32, value);
    }
}

impl GicCpuIf {
    unsafe fn read(&self, reg: u32) -> u32 {
        core::ptr::read_volatile((self.address + reg as usize) as *const u32)
    }

    unsafe fn write(&self, reg: u32, value: u32) {
        core::ptr::write_volatile((self.address + reg as usize) as *mut u32, value);
    }
}

pub fn init(gicc_base: usize, gicd_base: usize) -> IntController {
    let mut controller = IntController::new(gicc_base, gicd_base);
    controller.init();
    controller
}

pub fn get_irq_num(gicc_base: usize, gicd_base: usize) -> usize {
    IntController::new(gicc_base, gicd_base).pending_irq()
}
