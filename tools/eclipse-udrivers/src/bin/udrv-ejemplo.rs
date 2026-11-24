//! `udrv-ejemplo pci-VVVV-DDDD@N`: the smallest whole driver of a PCI
//! device, the one `docs/README-drivers.md` walks through. Built as a
//! `.drv` and installed from the control panel, it is what a new driver
//! starts from.
//!
//! It claims the device the kernel left it (`udriver=pci-VVVV-DDDD@N`),
//! maps its registers, and serves `/dev/udrv-ejemplo`: reading it gives a
//! line with the device's identity, the size of its window, its first
//! register and how many interrupts it has raised. A real driver puts its
//! own work where this one counts interrupts.

use std::process::exit;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use udrivers::claim::{self, Claim};
use udrivers::served::{self, Device, Node, EAGAIN, POLLIN};

const NAME: &str = "udrv-ejemplo";

/// Interrupts seen, counted by the thread that waits for them.
static IRQS: AtomicU64 = AtomicU64::new(0);

fn die(what: impl std::fmt::Display) -> ! {
    eprintln!("{NAME}: {what}");
    exit(1);
}

/// `pci-VVVV-DDDD@N`, as eclipse-init passes it: the vendor, the device
/// and which of the devices with that identity.
fn parse(item: &str) -> Option<(u16, u16, usize)> {
    let (ids, n) = item.strip_prefix("pci-")?.split_once('@')?;
    let (v, d) = ids.split_once('-')?;
    Some((
        u16::from_str_radix(v, 16).ok()?,
        u16::from_str_radix(d, 16).ok()?,
        n.parse().ok()?,
    ))
}

/// What `/dev/udrv-ejemplo` reads: one line per open.
struct Ejemplo {
    claim: Claim,
    regs: *mut u8,
    /// The opens that have not read their line yet.
    pending: Vec<u32>,
    next: u32,
}

// SAFETY: `regs` is the mapping of the device's window, which this process
// holds for its whole life; the node's thread is the only one that reads it.
unsafe impl Send for Ejemplo {}

impl Ejemplo {
    fn line(&self) -> String {
        // SAFETY: the window is at least four bytes when it is not empty,
        // and register reads must be volatile.
        let first = if self.claim.size >= 4 {
            unsafe { (self.regs as *const u32).read_volatile() }
        } else {
            0
        };
        format!(
            "pci {:04x}:{:04x} window {:#x} bytes reg0 {first:#010x} irqs {}\n",
            self.claim.pci_id >> 16,
            self.claim.pci_id & 0xffff,
            self.claim.size,
            IRQS.load(Ordering::Relaxed),
        )
    }
}

impl Device for Ejemplo {
    fn open(&mut self, _flags: u64) -> Result<(u32, u64), i32> {
        self.next += 1;
        self.pending.push(self.next);
        Ok((self.next, POLLIN))
    }

    fn release(&mut self, fh: u32) {
        self.pending.retain(|x| *x != fh);
    }

    fn read(&mut self, _node: &Node, fh: u32, _off: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let Some(at) = self.pending.iter().position(|x| *x == fh) else {
            return Ok(0); // the line was read: end of file
        };
        let line = self.line();
        if buf.len() < line.len() {
            return Err(EAGAIN);
        }
        self.pending.remove(at);
        buf[..line.len()].copy_from_slice(line.as_bytes());
        Ok(line.len())
    }

    fn write(&mut self, _: &Node, _: u32, _: u64, _: &[u8]) -> Result<usize, i32> {
        Err(served::EINVAL)
    }
}

fn main() {
    let item = std::env::args().nth(1).unwrap_or_default();
    let Some((vendor, device, index)) = parse(&item) else {
        die("uso: udrv-ejemplo pci-VVVV-DDDD@N");
    };
    // 1. Take the device. `ALREADY_BOUND` is retried a while: the driver
    //    before this one may have just died.
    let claim = claim::claim_pci(vendor, device, index, 100)
        .unwrap_or_else(|e| die(format!("no se pudo reclamar {item}: {e}")));
    // 2. Its registers: the first memory BAR, uncached.
    let regs = claim
        .map_regs()
        .unwrap_or_else(|e| die(format!("no se pudieron mapear los registros: {e}")));
    eprintln!(
        "{NAME}: {item} reclamado, ventana de {:#x} bytes, irq {}",
        claim.size,
        if claim.irq == u32::MAX {
            "ninguna".to_string()
        } else {
            claim.irq.to_string()
        }
    );
    // 3. Its interrupt, waited for on a thread of its own. A real driver
    //    reads the device's status here and quiets it.
    if let Some(irq) = claim.interrupt {
        std::thread::spawn(move || {
            while claim::interrupt_wait(irq).is_ok() {
                IRQS.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    // 4. What the rest of the system sees: a node in /dev.
    let node = Node::serve(NAME, 240, 9, 0o444).unwrap_or_else(|e| die(e));
    let dev = Mutex::new(Ejemplo {
        claim,
        regs,
        pending: Vec::new(),
        next: 0,
    });
    die(format!("el núcleo soltó /dev/{NAME}: {}", node.run(&dev)))
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn the_argument_is_the_item_eclipse_init_passes() {
        assert_eq!(parse("pci-14c3-0616@0"), Some((0x14c3, 0x0616, 0)));
        assert_eq!(parse("pci-8086-7aa3@2"), Some((0x8086, 0x7aa3, 2)));
        assert_eq!(parse("nvme@0"), None);
        assert_eq!(parse(""), None);
    }
}
