//! Devices the kernel leaves to a userspace driver.
//!
//! `udriver=e1000e@N` on the command line keeps the kernel's hands off the
//! `N`th e1000e it finds (counting from 0, in probe order): it is not
//! initialised, not registered, and its window and interrupt line are
//! recorded here instead. `udriver=virtio-net@N` does the same for a virtio
//! network card, which the kernel has no driver for at all: without the
//! reservation it is simply not looked at. Several devices
//! are named with commas: `udriver=virtio-blk@1,virtio-net@0`. A process can then claim
//! it and drive it from userspace (`eclipse_device_claim` in the loader's
//! Zircon gateway). One process at a time: the device goes back when its
//! driver dies, reset, and the next process to ask gets it.
//!
//! The count is per kind and runs across every bus the kernel probes, so
//! `virtio-blk@1` is "the second virtio disk" whatever the transport. The
//! kernel takes every device it has a driver for unless the command line
//! says otherwise, as it always has. A virtio network card is one it has
//! none for, so the first of them is left to userspace without a word
//! ([`wanted`]): there it is the card of the network stack that runs as a
//! process, where otherwise it would be nobody's.
//!
//! Every disk goes to userspace, named or not: the kernel has no driver for
//! an NVMe or an AHCI controller ([`crate::pci_disks`]), and drives one
//! virtio disk only, the root of an aarch64 machine with no initrd
//! ([`keep_boot_disk`]). Everywhere else the root comes from the initrd. And the first HD Audio controller that is not a
//! GPU's goes unasked ([`leave_first_hda`]): `uhda=kernel` keeps it. And
//! the first four virtio input devices and the PC's i8042 ([`leave_input`]),
//! whose events reach the consoles and `/sys` from their drivers too: the
//! kernel has no driver for either.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::{Mutex, Once};

/// A virtio block device.
pub const VIRTIO_BLK: &str = "virtio-blk";
/// A virtio network card.
pub const VIRTIO_NET: &str = "virtio-net";
/// An Intel 82574 network card (QEMU's `e1000e`), which the kernel drives
/// itself unless `udriver=e1000e@N` asks for it.
pub const E1000E: &str = "e1000e";
/// An NVMe controller, always left to userspace: the kernel has no
/// driver for it.
pub const NVME: &str = "nvme";
/// An AHCI (SATA) controller, with every disk on its ports, always left
/// to userspace: the kernel has no driver for it.
pub const AHCI: &str = "ahci";
/// A virtio input device (keyboard, mouse, tablet), always left to
/// userspace: the kernel has no driver for it. Its driver in userspace
/// serves it as `/dev/input/event*`.
pub const VIRTIO_INPUT: &str = "virtio-input";
/// The keyboard port of the PC's i8042 (PS/2) controller, always left to
/// userspace with the rest of the controller, this and [`I8042_AUX`], to
/// one driver there: the kernel has no driver for it.
pub const I8042: &str = "i8042";
/// The aux (mouse) port of the same controller: its own interrupt line,
/// the same two I/O ports.
pub const I8042_AUX: &str = "i8042-aux";
/// An Intel HD Audio controller, with every codec behind it, which the
/// kernel drives itself unless `udriver=hda@N` asks for it: its driver in
/// userspace serves it as `/dev/dsp`.
pub const HDA: &str = "hda";
/// An xHCI USB controller, with every device on its ports, which the kernel
/// drives itself unless `udriver=xhci@N` asks for it: its driver in
/// userspace serves the keyboards and pointers on it as
/// `/dev/input/eventN`.
pub const XHCI: &str = "xhci";
/// The kinds that can be reserved.
const KINDS: [&str; 10] = [
    VIRTIO_BLK,
    VIRTIO_NET,
    E1000E,
    NVME,
    AHCI,
    VIRTIO_INPUT,
    I8042,
    I8042_AUX,
    HDA,
    XHCI,
];

/// How the device is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Legacy virtio over MMIO (spec 1.1 §4.2): the whole register window.
    VirtioMmio,
    /// Modern virtio over PCI (spec 1.1 §4.1): the window is the memory
    /// BAR holding all four structures, at these offsets in it.
    VirtioPci {
        /// `VIRTIO_PCI_CAP_COMMON_CFG`.
        common: u32,
        /// `VIRTIO_PCI_CAP_NOTIFY_CFG`.
        notify: u32,
        /// `VIRTIO_PCI_CAP_ISR_CFG`.
        isr: u32,
        /// `VIRTIO_PCI_CAP_DEVICE_CFG`.
        device: u32,
        /// `notify_off_multiplier`.
        notify_multiplier: u32,
    },
    /// A PCI function that is not virtio: the window is its first memory
    /// BAR, the registers as its datasheet lays them out.
    Pci,
    /// I/O ports, x86's: the window is `size` ports from port `paddr`, which
    /// the kernel reads and writes for the driver ([`UserDevice::port_in`]),
    /// a process having no way of its own. No memory goes with it.
    Ports,
}

/// A device reserved for userspace.
#[derive(Debug)]
pub struct UserDevice {
    /// The kind, as the command line names it.
    pub kind: &'static str,
    /// Its place among the devices of its kind (the `N` of `kind@N`).
    pub index: usize,
    /// How its registers are laid out.
    pub transport: Transport,
    /// Physical address of its register window.
    pub paddr: usize,
    /// Size of that window, in bytes.
    pub size: usize,
    /// Its interrupt, as the number its controller's `IrqScheme` takes,
    /// when known: the line on a GIC or PLIC, the vector of an MSI on x86.
    pub irq: Option<usize>,
    /// Where the kernel itself reaches that window, when the bus that found
    /// the device mapped it. Only [`reset`](Self::reset) goes through it.
    pub vaddr: Option<usize>,
    claimed: AtomicBool,
}

/// What a bus knows of a device it hands over beyond its window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wiring {
    /// See [`UserDevice::irq`].
    pub irq: Option<usize>,
    /// See [`UserDevice::vaddr`].
    pub vaddr: Option<usize>,
}

/// `Status` in a virtio-mmio window (spec 1.1 §4.2.2), legacy or not.
const MMIO_STATUS: usize = 0x70;
/// `device_status` in the common configuration structure of virtio over
/// PCI (spec 1.1 §4.1.4.3).
const PCI_DEVICE_STATUS: usize = 0x14;
/// How many times the status is read back after a reset before the device
/// is given up on. The spec asks the driver to wait for the zero (§4.1.4.3.2);
/// QEMU answers it on the first read.
const RESET_READS: usize = 1000;
/// An e1000e's registers (82574 datasheet §10.2): `CTRL`, `RCTL`, `TCTL`
/// and `IMC`, by byte offset, and the bit of `CTRL` that resets the MAC.
const E1000E_CTRL: usize = 0x0000;
const E1000E_IMC: usize = 0x00d8;
const E1000E_RCTL: usize = 0x0100;
const E1000E_TCTL: usize = 0x0400;
const E1000E_CTRL_RST: u32 = 1 << 26;
/// How far into its window the registers above go.
const E1000E_RESET_SPAN: usize = E1000E_TCTL + 4;
/// An NVMe controller's registers (NVMe 1.4 §3.1): `CC` and `CSTS`, by
/// byte offset, and the bit of each that enables it and says it is ready.
const NVME_CC: usize = 0x14;
const NVME_CSTS: usize = 0x1c;
const NVME_CC_EN: u32 = 1;
const NVME_CSTS_RDY: u32 = 1;
/// How many times `CSTS` is read back after `CC.EN` is cleared. A real
/// controller may take up to `CAP.TO` half-seconds; QEMU's is done at once.
const NVME_RESET_READS: usize = 1_000_000;
/// The i8042's two ports: data at `0x60`, status and commands at `0x64`,
/// and the bit of the status that says the controller has not yet taken
/// the last byte written to it.
pub const I8042_PORT: usize = 0x60;
pub const I8042_SPAN: usize = 5;
const I8042_STATUS: usize = 4;
const I8042_INPUT_FULL: u32 = 1 << 1;
/// The ISA lines of its two ports.
pub const I8042_KBD_IRQ: usize = 1;
pub const I8042_AUX_IRQ: usize = 12;
/// An AHCI controller's global control register (AHCI 1.3.1 §3.1.2) and
/// its HBA reset bit, which the controller clears when it is done.
const AHCI_GHC: usize = 0x04;
const AHCI_GHC_HR: u32 = 1;
/// An HDA controller's capabilities, the run bytes of its two command
/// rings, its stream descriptors (0x20 bytes each, control byte first) and
/// its global control register with the controller reset bit, which reads
/// 0 once the controller is in reset (HDA 1.0a §3.3).
const HDA_GCAP: usize = 0x00;
const HDA_GCTL: usize = 0x08;
const HDA_GCTL_CRST: u32 = 1;
const HDA_CORBCTL: usize = 0x4c;
const HDA_RIRBCTL: usize = 0x5c;
const HDA_SD_BASE: usize = 0x80;
const HDA_SD_LEN: usize = 0x20;
/// An xHCI controller's capability length (the operational registers
/// follow), its command register with the run/stop and reset bits, and its
/// status register, whose halted bit is set once the controller has
/// stopped (xHCI 1.2 §5.4.1, §5.4.2).
const XHCI_CAPLENGTH: usize = 0x00;
const XHCI_USBCMD: usize = 0x00;
const XHCI_USBSTS: usize = 0x04;
const XHCI_CMD_RUN: u32 = 1;
const XHCI_CMD_RESET: u32 = 1 << 1;
const XHCI_CMD_INTE: u32 = 1 << 2;
const XHCI_STS_HALTED: u32 = 1;

impl UserDevice {
    /// Take the device. Only one caller gets `true` until it is
    /// [released](Self::release): the device has one driver at a time.
    pub fn claim(&self) -> bool {
        !self.claimed.swap(true, Ordering::AcqRel)
    }

    /// Whether a driver has taken it.
    pub fn is_claimed(&self) -> bool {
        self.claimed.load(Ordering::Acquire)
    }

    /// Stop the device: write zero to its status register, which is how a
    /// virtio device is reset (spec 1.1 §2.1.1), and wait for it to read
    /// back zero. From then on it touches none of the memory its queues
    /// named and raises no interrupt, until a driver sets it up again.
    ///
    /// An e1000e is stopped as its own driver stops it: every interrupt
    /// masked, receive and transmit off, then a MAC reset (`CTRL.RST`),
    /// which the card clears when it is done. Its rings are forgotten with
    /// it: the next driver programs them again.
    ///
    /// `false` when it cannot be done -- the kernel has no mapping of the
    /// window, the window is too short to hold the register, or the device
    /// never answered zero -- and then the device must be taken to be
    /// running still.
    pub fn reset(&self) -> bool {
        if self.transport == Transport::Ports {
            return self.reset_ports();
        }
        let Some(vaddr) = self.vaddr else {
            return false;
        };
        match self.transport {
            Transport::VirtioMmio => {
                if self.size < MMIO_STATUS + 4 {
                    return false;
                }
                let status = (vaddr + MMIO_STATUS) as *mut u32;
                // SAFETY: `vaddr` is the bus's mapping of the `size` bytes
                // of the window, and the register is inside it.
                unsafe { status.write_volatile(0) };
                (0..RESET_READS).any(|_| unsafe { status.read_volatile() } == 0)
            }
            Transport::VirtioPci { common, .. } => {
                let at = common as usize + PCI_DEVICE_STATUS;
                if self.size <= at {
                    return false;
                }
                let status = (vaddr + at) as *mut u8;
                // SAFETY: as above; `common` was checked against the BAR
                // when the device was recorded.
                unsafe { status.write_volatile(0) };
                (0..RESET_READS).any(|_| unsafe { status.read_volatile() } == 0)
            }
            Transport::Pci if self.kind == E1000E => {
                if self.size < E1000E_RESET_SPAN {
                    return false;
                }
                let reg = |off: usize| (vaddr + off) as *mut u32;
                // SAFETY: as above; every register is inside the window.
                unsafe {
                    reg(E1000E_IMC).write_volatile(u32::MAX);
                    reg(E1000E_RCTL).write_volatile(0);
                    reg(E1000E_TCTL).write_volatile(0);
                    let ctrl = reg(E1000E_CTRL).read_volatile();
                    reg(E1000E_CTRL).write_volatile(ctrl | E1000E_CTRL_RST);
                }
                let done = (0..RESET_READS)
                    .any(|_| unsafe { reg(E1000E_CTRL).read_volatile() } & E1000E_CTRL_RST == 0);
                // The reset unmasks nothing, but leaves whatever the card
                // had raised: masked again, for the next driver to start
                // from silence.
                // SAFETY: as above.
                unsafe { reg(E1000E_IMC).write_volatile(u32::MAX) };
                done
            }
            Transport::Pci if self.kind == NVME => {
                if self.size < NVME_CSTS + 4 {
                    return false;
                }
                let reg = |off: usize| (vaddr + off) as *mut u32;
                // SAFETY: as above; both registers are inside the window.
                // Clearing `CC.EN` is the controller reset (§7.6.2): every
                // queue is gone and nothing more is read or written of the
                // memory they named.
                unsafe {
                    let cc = reg(NVME_CC).read_volatile();
                    reg(NVME_CC).write_volatile(cc & !NVME_CC_EN);
                }
                (0..NVME_RESET_READS)
                    .any(|_| unsafe { reg(NVME_CSTS).read_volatile() } & NVME_CSTS_RDY == 0)
            }
            Transport::Pci if self.kind == AHCI => {
                if self.size < AHCI_GHC + 4 {
                    return false;
                }
                let ghc = (vaddr + AHCI_GHC) as *mut u32;
                // SAFETY: as above. The HBA reset (§10.4.3) stops every port
                // and forgets their command lists: nothing more is read or
                // written of the memory they named.
                unsafe { ghc.write_volatile(ghc.read_volatile() | AHCI_GHC_HR) };
                (0..NVME_RESET_READS).any(|_| unsafe { ghc.read_volatile() } & AHCI_GHC_HR == 0)
            }
            Transport::Pci if self.kind == HDA => {
                if self.size < HDA_RIRBCTL + 1 {
                    return false;
                }
                // SAFETY: as above; every register written is inside the
                // window, which holds the stream descriptors GCAP counts.
                // Every DMA engine is stopped first, as the spec asks
                // before a reset: the rings and each stream, which then
                // read or write nothing more of the memory they named.
                unsafe {
                    let gcap = ((vaddr + HDA_GCAP) as *const u16).read_volatile() as usize;
                    let streams = ((gcap >> 12) & 0xf) + ((gcap >> 8) & 0xf) + ((gcap >> 3) & 0x1f);
                    ((vaddr + HDA_CORBCTL) as *mut u8).write_volatile(0);
                    ((vaddr + HDA_RIRBCTL) as *mut u8).write_volatile(0);
                    for n in 0..streams {
                        let ctl = HDA_SD_BASE + n * HDA_SD_LEN;
                        if ctl < self.size {
                            ((vaddr + ctl) as *mut u8).write_volatile(0);
                        }
                    }
                    let gctl = (vaddr + HDA_GCTL) as *mut u32;
                    gctl.write_volatile(gctl.read_volatile() & !HDA_GCTL_CRST);
                }
                let gctl = (vaddr + HDA_GCTL) as *const u32;
                // SAFETY: as above.
                (0..NVME_RESET_READS).any(|_| unsafe { gctl.read_volatile() } & HDA_GCTL_CRST == 0)
            }
            Transport::Pci if self.kind == XHCI => {
                // SAFETY: as above; the capability length is the first
                // byte of the window.
                let op = vaddr
                    + unsafe { ((vaddr + XHCI_CAPLENGTH) as *const u8).read_volatile() } as usize;
                if op + XHCI_USBSTS + 4 > vaddr + self.size {
                    return false;
                }
                let cmd = (op + XHCI_USBCMD) as *mut u32;
                let sts = (op + XHCI_USBSTS) as *const u32;
                // SAFETY: both registers are inside the window. Stopped,
                // the controller walks no ring and writes no event; then
                // it is reset, which forgets every ring and context the
                // driver gave it. The next driver waits for the reset to
                // end, as it would after a firmware's.
                unsafe {
                    cmd.write_volatile(cmd.read_volatile() & !(XHCI_CMD_RUN | XHCI_CMD_INTE));
                }
                let halted = (0..NVME_RESET_READS)
                    .any(|_| unsafe { sts.read_volatile() } & XHCI_STS_HALTED != 0);
                if halted {
                    // SAFETY: as above.
                    unsafe { cmd.write_volatile(XHCI_CMD_RESET) };
                }
                halted
            }
            // A PCI function of a kind this does not know how to stop.
            Transport::Pci => false,
            Transport::Ports => self.reset_ports(),
        }
    }

    /// Stop a device reached by I/O ports. An i8042 port is disabled
    /// (`0xAD` for the keyboard, `0xA7` for the aux port): it sends no more
    /// bytes and raises no more interrupts until a driver enables it again.
    fn reset_ports(&self) -> bool {
        if ![I8042, I8042_AUX].contains(&self.kind) || self.size < I8042_SPAN {
            return false;
        }
        let disable = if self.kind == I8042 { 0xad } else { 0xa7 };
        let ready = (0..RESET_READS).any(|_| {
            self.port_in(I8042_STATUS, 1)
                .is_some_and(|s| s & I8042_INPUT_FULL == 0)
        });
        ready && self.port_out(I8042_STATUS, 1, disable)
    }

    /// Read `width` bytes (1, 2 or 4) at port `offset` of the device's
    /// window. `None` when that is not inside it, the device is not reached
    /// by ports, or this machine has none.
    pub fn port_in(&self, offset: usize, width: usize) -> Option<u32> {
        let port = self.port(offset, width)?;
        #[cfg(target_arch = "x86_64")]
        {
            use x86_64::instructions::port::Port;
            // SAFETY: a port of the device's own window, which the kernel
            // left to whoever drives it.
            Some(unsafe {
                match width {
                    1 => Port::<u8>::new(port).read() as u32,
                    2 => Port::<u16>::new(port).read() as u32,
                    _ => Port::<u32>::new(port).read(),
                }
            })
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = port;
            None
        }
    }

    /// Write `width` bytes of `value` at port `offset`, as
    /// [`port_in`](Self::port_in) reads. Whether it was done.
    pub fn port_out(&self, offset: usize, width: usize, value: u32) -> bool {
        let Some(port) = self.port(offset, width) else {
            return false;
        };
        #[cfg(target_arch = "x86_64")]
        {
            use x86_64::instructions::port::Port;
            // SAFETY: as `port_in`.
            unsafe {
                match width {
                    1 => Port::<u8>::new(port).write(value as u8),
                    2 => Port::<u16>::new(port).write(value as u16),
                    _ => Port::<u32>::new(port).write(value),
                }
            }
            true
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (port, value);
            false
        }
    }

    /// The port `offset` names, when `width` bytes there are the device's.
    fn port(&self, offset: usize, width: usize) -> Option<u16> {
        let inside = self.transport == Transport::Ports
            && matches!(width, 1 | 2 | 4)
            && offset
                .checked_add(width)
                .is_some_and(|end| end <= self.size);
        let port = self.paddr.checked_add(offset)?;
        (inside && port + width <= 0x1_0000).then_some(port as u16)
    }

    /// Hand the device back: the next [`claim`](Self::claim) gets it. For
    /// whoever took it, once it is [reset](Self::reset) and nothing of the
    /// last driver's is left on it.
    pub fn release(&self) {
        self.claimed.store(false, Ordering::Release);
    }
}

/// What the command line asked for: each a `(kind, index)`.
static WANTED: Once<Vec<(&'static str, usize)>> = Once::new();
/// Devices of each of [`KINDS`] seen so far, reserved or not.
static SEEN: [AtomicUsize; KINDS.len()] = [const { AtomicUsize::new(0) }; KINDS.len()];
static RESERVED: Mutex<Vec<Arc<UserDevice>>> = Mutex::new(Vec::new());
/// Whether the first e1000e is left to userspace without the boot naming
/// it: see [`leave_first_e1000e`].
static E1000E_UNASKED: AtomicBool = AtomicBool::new(false);

/// Whether a boot whose `udriver=` value is `spec` leaves the first e1000e
/// to userspace unasked, on a machine where that is done: it does unless
/// the boot names a network card itself, which is then the one it means.
pub fn e1000e_unasked(spec: Option<&str>) -> bool {
    !parse_specs(spec.unwrap_or(""))
        .iter()
        .any(|(kind, _)| [VIRTIO_NET, E1000E].contains(kind))
}

/// Leave the first e1000e to userspace without a word, for the network
/// stack that runs as a process to drive, as the first virtio network
/// card is left: `spec` is the boot's `udriver=` value
/// ([`e1000e_unasked`]). The kernel has a driver for this card, so whoever
/// finds it can still keep it ([`keep_first_e1000e`]). Before the buses
/// are probed.
pub fn leave_first_e1000e(spec: Option<&str>) {
    if e1000e_unasked(spec) {
        info!(
            "udriver: e1000e #0, if it is an 82574 and there is no virtio card, is for userspace"
        );
        E1000E_UNASKED.store(true, Ordering::Release);
    }
}

/// Whether the first e1000e would be left to userspace unasked.
pub fn first_e1000e_unasked() -> bool {
    E1000E_UNASKED.load(Ordering::Acquire)
}

/// The kernel keeps the first e1000e after all, unless the boot named it:
/// it is a part the userspace driver does not take, or the machine has a
/// virtio card for the stack process.
pub fn keep_first_e1000e() {
    E1000E_UNASKED.store(false, Ordering::Release);
}

/// Whether the first virtio disk is the kernel's: the disk an aarch64
/// machine with no initrd mounts its root from ([`keep_boot_disk`]).
static KEEP_BOOT_DISK: AtomicBool = AtomicBool::new(false);

/// Keep the first virtio disk for the kernel's own driver, the one disk it
/// still drives: on a machine with no initrd (QEMU `virt` on aarch64, whose
/// bootloader has no way to hand one over) the root is on it, and it is
/// mounted before any driver in userspace can run. Before the buses are
/// probed. Every other virtio disk goes to userspace, as on every other
/// machine.
pub fn keep_boot_disk(boot_disk: bool) {
    KEEP_BOOT_DISK.store(boot_disk, Ordering::Release);
}

/// Whether a boot whose `udriver=` value is `spec` leaves every device of
/// `kind` to userspace unasked: it does unless it names one of them, and
/// then that one is what it means.
fn kind_unasked(spec: Option<&str>, kind: &str) -> bool {
    !parse_specs(spec.unwrap_or(""))
        .iter()
        .any(|(k, _)| *k == kind)
}

/// Whether the first HD Audio controller that is not a GPU's is left to
/// userspace without the boot naming it: see [`leave_first_hda`].
static HDA_UNASKED: AtomicBool = AtomicBool::new(false);

/// Leave the first HD Audio controller that is not on a GPU to its driver
/// in userspace, which serves it as `/dev/dsp` and `/dev/snd`, unless the
/// boot names an `hda@N` itself. One: init starts one driver, for the
/// first `hda@` of `/proc/udrivers`. Not a GPU's HDMI function, which that
/// driver has no output for. Before the buses are probed.
pub fn leave_first_hda(spec: Option<&str>) {
    if kind_unasked(spec, HDA) {
        info!("udriver: the first HD Audio controller is for userspace");
        HDA_UNASKED.store(true, Ordering::Release);
    }
}

/// An HD Audio controller was found (`gpu`: the HDMI function of a GPU):
/// whether it is left to userspace, as [`reserve_if`] says, or unasked as
/// the first that is not a GPU's ([`leave_first_hda`]).
pub fn reserve_hda(
    gpu: bool,
    transport: Transport,
    paddr: usize,
    size: usize,
    wiring: impl FnOnce() -> Wiring,
) -> bool {
    let unasked = !gpu && HDA_UNASKED.swap(false, Ordering::AcqRel);
    reserve_with(HDA, true, unasked, transport, paddr, size, wiring)
}

/// Whether the virtio input devices are left to userspace without the
/// boot naming them: see [`leave_input`].
static INPUT_UNASKED: AtomicBool = AtomicBool::new(false);
/// The same for the i8042.
static I8042_UNASKED: AtomicBool = AtomicBool::new(false);
/// How many virtio input devices go unasked: init has a service for each
/// of the first four (`uinput` to `uinput4`), and a fifth would have no
/// driver. The kernel has none either: a fifth is nobody's.
const INPUT_UNASKED_MAX: usize = 4;

/// Leave the first four virtio input devices and the PC's i8042, keyboard
/// and mouse, to their drivers in userspace (with `gateway`: the gateway
/// they claim them through is open), whether the boot names them or not.
/// The kernel has no driver for either. Their events still reach the
/// consoles, and libinput finds them in `/sys`, as a kernel device's
/// (`linux_object::fs::devfs::served::input`). Before the buses are probed.
pub fn leave_input(gateway: bool) {
    if gateway {
        info!("udriver: the virtio input devices and the i8042 are for userspace");
        INPUT_UNASKED.store(true, Ordering::Release);
        I8042_UNASKED.store(true, Ordering::Release);
    } else {
        warn!("udriver: zircon.gateway=0: no driver claims a virtio input device or the i8042, and the kernel has none");
    }
}

/// Parse one item of a `udriver=` value: `virtio-blk@N`, `virtio-net@N`,
/// `e1000e@N`, `nvme@N`, `ahci@N`, `virtio-input@N`, `i8042@0`, `hda@N` or
/// `xhci@N`.
/// Anything else reserves nothing (and says so). Not `kind:N`: the command
/// line is split on `:`.
pub fn parse_spec(spec: &str) -> Option<(&'static str, usize)> {
    let (kind, index) = spec.split_once('@')?;
    let kind = KINDS.iter().find(|k| **k == kind)?;
    let index = index.parse().ok()?;
    Some((kind, index))
}

/// Parse a whole `udriver=` value: items separated by commas. The ones
/// that name nothing reservable are left out.
pub fn parse_specs(spec: &str) -> Vec<(&'static str, usize)> {
    spec.split(',').filter_map(parse_spec).collect()
}

/// What a boot leaves to userspace: the devices its `udriver=` value
/// names and, with `net_by_default`, the first virtio network card when
/// that value names no network card at all. Each with whether the command
/// line named it.
pub fn wanted(spec: Option<&str>, net_by_default: bool) -> Vec<((&'static str, usize), bool)> {
    let mut wanted: Vec<_> = parse_specs(spec.unwrap_or(""))
        .into_iter()
        .map(|device| (device, true))
        .collect();
    if net_by_default && !wanted.iter().any(|((kind, _), _)| *kind == VIRTIO_NET) {
        wanted.push(((VIRTIO_NET, 0), false));
    }
    wanted
}

/// Set the reservation from the command line's `udriver=` value, before
/// the buses are probed: see [`wanted`]. Only the first call counts.
pub fn set_reservation(spec: Option<&str>, net_by_default: bool) {
    WANTED.call_once(|| {
        for item in spec.unwrap_or("").split(',').filter(|s| !s.is_empty()) {
            if parse_spec(item).is_none() {
                warn!("udriver={item}: not a device this kernel can reserve");
            }
        }
        wanted(spec, net_by_default)
            .into_iter()
            .map(|((kind, index), named)| {
                if named {
                    info!("udriver: reserving {kind} #{index} for userspace");
                } else {
                    info!("udriver: {kind} #{index}, if there is one, is for userspace: the kernel has no driver for it");
                }
                (kind, index)
            })
            .collect()
    });
}

/// A bus found a virtio block device: count it, and answer whether it is
/// the one to leave alone. When it is, it is recorded and the caller must
/// not touch it -- not even to reset it.
pub fn reserve_virtio_blk(transport: Transport, paddr: usize, size: usize, wiring: Wiring) -> bool {
    reserve_if(VIRTIO_BLK, true, transport, paddr, size, || wiring)
}

/// A bus found a virtio network card: count it, and answer whether it is
/// one to hand to userspace. The kernel drives none of them either way.
pub fn reserve_virtio_net(transport: Transport, paddr: usize, size: usize, wiring: Wiring) -> bool {
    reserve_if(VIRTIO_NET, true, transport, paddr, size, || wiring)
}

/// A bus found a virtio input device: count it, and answer whether it is
/// the one to leave alone. When it is, the caller must not touch it: the
/// kernel's own driver would reset it and take its events.
pub fn reserve_virtio_input(
    transport: Transport,
    paddr: usize,
    size: usize,
    wiring: Wiring,
) -> bool {
    reserve_if(VIRTIO_INPUT, true, transport, paddr, size, || wiring)
}

/// The PC found an i8042: whether the boot leaves it to userspace
/// (`udriver=i8042@0`). When it does both its ports are recorded, the
/// keyboard as [`I8042`] and the mouse as [`I8042_AUX`], each with its
/// line, and the caller must not touch the controller.
pub fn reserve_i8042() -> bool {
    if !I8042_UNASKED.load(Ordering::Acquire)
        && !WANTED
            .get()
            .is_some_and(|w| w.contains(&(I8042, 0)) || w.contains(&(I8042_AUX, 0)))
    {
        return false;
    }
    for (kind, irq) in [(I8042, I8042_KBD_IRQ), (I8042_AUX, I8042_AUX_IRQ)] {
        info!("udriver: {kind} #0 at port {I8042_PORT:#x} (irq {irq}) left to userspace");
        RESERVED.lock().push(Arc::new(UserDevice {
            kind,
            index: 0,
            transport: Transport::Ports,
            paddr: I8042_PORT,
            size: I8042_SPAN,
            irq: Some(irq),
            vaddr: None,
            claimed: AtomicBool::new(false),
        }));
    }
    true
}

/// [`reserve_virtio_blk`] for a disk that may not be fit to hand over
/// (`usable` false: a PCI disk without a modern transport). It is counted
/// all the same, so the index of every later disk stays put, and the
/// kernel keeps it. `wiring` is asked only for the disk that is handed
/// over, so a bus can set up the interrupt of that one disk and no other.
pub fn reserve_virtio_blk_if(
    usable: bool,
    transport: Transport,
    paddr: usize,
    size: usize,
    wiring: impl FnOnce() -> Wiring,
) -> bool {
    reserve_if(VIRTIO_BLK, usable, transport, paddr, size, wiring)
}

/// The one counter-and-record behind every `reserve_*`: `kind` is one of
/// [`KINDS`].
pub fn reserve_if(
    kind: &'static str,
    usable: bool,
    transport: Transport,
    paddr: usize,
    size: usize,
    wiring: impl FnOnce() -> Wiring,
) -> bool {
    reserve_with(kind, usable, false, transport, paddr, size, wiring)
}

/// [`reserve_if`], where `unasked` also leaves the device to userspace.
fn reserve_with(
    kind: &'static str,
    usable: bool,
    unasked: bool,
    transport: Transport,
    paddr: usize,
    size: usize,
    wiring: impl FnOnce() -> Wiring,
) -> bool {
    let Some(slot) = KINDS.iter().position(|k| *k == kind) else {
        return false;
    };
    let index = SEEN[slot].fetch_add(1, Ordering::AcqRel);
    let wanted = unasked
        || WANTED.get().is_some_and(|w| w.contains(&(kind, index)))
        || (kind == E1000E && index == 0 && first_e1000e_unasked())
        // The kernel has no disk driver but for the boot disk of an aarch64
        // machine with no initrd ([`keep_boot_disk`]): a virtio disk, an
        // NVMe or an AHCI controller (`crate::pci_disks`) is userspace's or
        // nobody's.
        || (kind == VIRTIO_BLK && !(index == 0 && KEEP_BOOT_DISK.load(Ordering::Acquire)))
        || kind == NVME
        || kind == AHCI
        || (kind == VIRTIO_INPUT
            && index < INPUT_UNASKED_MAX
            && INPUT_UNASKED.load(Ordering::Acquire));
    if !wanted {
        return false;
    }
    if !usable {
        warn!("udriver: {kind} #{index} has no transport a userspace driver can use; the kernel keeps it");
        return false;
    }
    let Wiring { irq, vaddr } = wiring();
    info!("udriver: {kind} #{index} at {paddr:#x} (+{size:#x}, irq {irq:?}) left to userspace");
    if vaddr.is_none() {
        warn!("udriver: {kind} #{index} is not mapped in the kernel: it cannot be reset, so it will have one driver and no second");
    }
    RESERVED.lock().push(Arc::new(UserDevice {
        kind,
        index,
        transport,
        paddr,
        size,
        irq,
        vaddr,
        claimed: AtomicBool::new(false),
    }));
    true
}

/// The devices left to userspace, as `udriver=` would name each
/// (`virtio-net@0`), in the order the buses found them.
pub fn reserved() -> Vec<alloc::string::String> {
    RESERVED
        .lock()
        .iter()
        .map(|d| alloc::format!("{}@{}", d.kind, d.index))
        .collect()
}

/// The reserved device of `kind` with this index, if there is one.
pub fn find(kind: &str, index: usize) -> Option<Arc<UserDevice>> {
    RESERVED
        .lock()
        .iter()
        .find(|d| d.kind == kind && d.index == index)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_names_a_kind_and_an_index() {
        assert_eq!(parse_spec("virtio-blk@1"), Some((VIRTIO_BLK, 1)));
        assert_eq!(parse_spec("virtio-blk@0"), Some((VIRTIO_BLK, 0)));
        assert_eq!(parse_spec("virtio-blk"), None);
        assert_eq!(parse_spec("virtio-blk@x"), None);
        assert_eq!(parse_spec("virtio-net@0"), Some((VIRTIO_NET, 0)));
        assert_eq!(parse_spec("e1000e@0"), Some((E1000E, 0)));
        assert_eq!(parse_spec("nvme@1"), Some((NVME, 1)));
        assert_eq!(parse_spec("ahci@1"), Some((AHCI, 1)));
        assert_eq!(parse_spec("hda@0"), Some((HDA, 0)));
        assert_eq!(parse_spec("xhci@0"), Some((XHCI, 0)));
        assert_eq!(parse_spec("virtio-input@0"), Some((VIRTIO_INPUT, 0)));
        assert_eq!(parse_spec("i8042@0"), Some((I8042, 0)));
        assert_eq!(parse_spec("virtio-gpu@0"), None);
        assert_eq!(parse_spec(""), None);
    }

    #[test]
    fn the_first_e1000e_goes_unasked_unless_the_boot_names_a_card() {
        assert!(e1000e_unasked(None));
        assert!(e1000e_unasked(Some("")));
        assert!(e1000e_unasked(Some("virtio-blk@1")));
        assert!(!e1000e_unasked(Some("virtio-net@0")));
        assert!(!e1000e_unasked(Some("virtio-blk@1,e1000e@1")));
        assert!(!e1000e_unasked(Some("e1000e@0")));
    }

    #[test]
    fn several_devices_are_named_with_commas() {
        assert_eq!(
            parse_specs("virtio-blk@1,virtio-net@0"),
            [(VIRTIO_BLK, 1), (VIRTIO_NET, 0)]
        );
        assert_eq!(parse_specs("virtio-blk@1"), [(VIRTIO_BLK, 1)]);
        assert_eq!(parse_specs("nonsense,virtio-net@2"), [(VIRTIO_NET, 2)]);
        assert!(parse_specs("").is_empty());
    }

    /// A device whose window is `window`, a stand-in for its registers.
    fn device(transport: Transport, window: &mut [u8]) -> UserDevice {
        UserDevice {
            kind: VIRTIO_NET,
            index: 0,
            transport,
            paddr: 0x1000,
            size: window.len(),
            irq: None,
            vaddr: Some(window.as_mut_ptr() as usize),
            claimed: AtomicBool::new(false),
        }
    }

    #[test]
    fn a_device_has_one_driver_at_a_time() {
        let dev = device(Transport::VirtioMmio, &mut [0; 0x200]);
        assert!(!dev.is_claimed());
        assert!(dev.claim());
        assert!(!dev.claim());
        assert!(dev.is_claimed());
        dev.release();
        assert!(!dev.is_claimed());
        assert!(
            dev.claim(),
            "a released device is there for the next driver"
        );
        assert!(!dev.claim());
    }

    #[test]
    fn a_reset_zeroes_the_status_of_an_mmio_device() {
        // Aligned like the registers are: the status is read as a `u32`.
        let mut window = [0u32; 0x80];
        window[MMIO_STATUS / 4] = 0xf; // ACKNOWLEDGE | DRIVER | DRIVER_OK | FEATURES_OK
        window[MMIO_STATUS / 4 + 1] = 0x55;
        let bytes =
            unsafe { core::slice::from_raw_parts_mut(window.as_mut_ptr() as *mut u8, 0x200) };
        assert!(device(Transport::VirtioMmio, bytes).reset());
        assert_eq!(window[MMIO_STATUS / 4], 0);
        assert_eq!(window[MMIO_STATUS / 4 + 1], 0x55, "only the status");
    }

    #[test]
    fn a_reset_zeroes_the_status_byte_of_a_pci_device() {
        let transport = Transport::VirtioPci {
            common: 0x100,
            notify: 0x300,
            isr: 0x200,
            device: 0x400,
            notify_multiplier: 4,
        };
        let mut window = [0xffu8; 0x500];
        assert!(device(transport, &mut window).reset());
        let at = 0x100 + PCI_DEVICE_STATUS;
        assert_eq!(window[at], 0);
        assert_eq!((window[at - 1], window[at + 1]), (0xff, 0xff), "one byte");
    }

    #[test]
    fn an_e1000e_is_reset_with_its_interrupts_masked_and_its_queues_off() {
        let mut window = [0u32; E1000E_RESET_SPAN / 4];
        window[E1000E_RCTL / 4] = 0x0400_8002; // EN | BAM | SECRC
        window[E1000E_TCTL / 4] = 0x0103_f0fa;
        window[E1000E_CTRL / 4] = 0x0000_0040; // SLU
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(window.as_mut_ptr() as *mut u8, E1000E_RESET_SPAN)
        };
        let card = UserDevice {
            kind: E1000E,
            ..device(Transport::Pci, bytes)
        };
        // A window of plain memory keeps the bit set: the card is the one
        // that clears it, so the reset is not taken to have happened.
        assert!(!card.reset());
        assert_eq!(window[E1000E_IMC / 4], u32::MAX);
        assert_eq!((window[E1000E_RCTL / 4], window[E1000E_TCTL / 4]), (0, 0));
        assert_eq!(window[E1000E_CTRL / 4], 0x40 | E1000E_CTRL_RST);
    }

    #[test]
    fn an_xhci_controller_is_stopped_then_reset() {
        let mut words = [0u32; 0x20];
        // SAFETY: the words' bytes, aligned as the registers are.
        let window =
            unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, 0x80) };
        window[XHCI_CAPLENGTH] = 0x20;
        let op = 0x20;
        window[op..op + 4].copy_from_slice(&(XHCI_CMD_RUN | XHCI_CMD_INTE | 0x100).to_le_bytes());
        window[op + 4..op + 8].copy_from_slice(&XHCI_STS_HALTED.to_le_bytes());
        let ctrl = UserDevice {
            kind: XHCI,
            ..device(Transport::Pci, window)
        };
        assert!(ctrl.reset());
        assert_eq!(window[op..op + 4], XHCI_CMD_RESET.to_le_bytes());
        // One that never halts is not taken for stopped.
        window[op + 4] = 0;
        assert!(!ctrl.reset());
    }

    #[test]
    fn an_hda_controller_stops_its_rings_and_streams_then_goes_into_reset() {
        let mut words = [0u32; 0x40];
        // SAFETY: the words' bytes, aligned as the registers are.
        let window =
            unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, 0x100) };
        // One output stream, one input, no bidirectional.
        window[HDA_GCAP..HDA_GCAP + 2].copy_from_slice(&0x1100u16.to_le_bytes());
        window[HDA_GCTL] = 0x01;
        window[HDA_CORBCTL] = 0x02;
        window[HDA_RIRBCTL] = 0x03;
        window[HDA_SD_BASE] = 0x02;
        window[HDA_SD_BASE + HDA_SD_LEN] = 0x02;
        window[HDA_SD_BASE + 2] = 0x40; // the stream tag: not the run byte
        let ctrl = UserDevice {
            kind: HDA,
            ..device(Transport::Pci, window)
        };
        assert!(ctrl.reset());
        assert_eq!(window[HDA_GCTL], 0);
        assert_eq!((window[HDA_CORBCTL], window[HDA_RIRBCTL]), (0, 0));
        assert_eq!(
            (window[HDA_SD_BASE], window[HDA_SD_BASE + HDA_SD_LEN]),
            (0, 0)
        );
        assert_eq!(window[HDA_SD_BASE + 2], 0x40);
    }

    #[test]
    fn an_nvme_controller_is_reset_by_clearing_its_enable_bit() {
        let mut window = [0u32; 0x40 / 4];
        window[NVME_CC / 4] = 0x0046_0001; // IOSQES, IOCQES, EN
        window[NVME_CSTS / 4] = 0; // plain memory: "ready" already clear
        let bytes =
            unsafe { core::slice::from_raw_parts_mut(window.as_mut_ptr() as *mut u8, 0x40) };
        let ctrl = UserDevice {
            kind: NVME,
            ..device(Transport::Pci, bytes)
        };
        assert!(ctrl.reset());
        assert_eq!(window[NVME_CC / 4], 0x0046_0000, "only EN");
        // A window too short for CSTS is not touched.
        let mut short = [0xffu8; NVME_CSTS];
        let ctrl = UserDevice {
            kind: NVME,
            ..device(Transport::Pci, &mut short)
        };
        assert!(!ctrl.reset());
        assert!(short.iter().all(|b| *b == 0xff));
    }

    #[test]
    fn an_ahci_controller_is_reset_with_its_hba_reset_bit() {
        let mut window = [0u32; 0x10 / 4];
        window[AHCI_GHC / 4] = 0x8000_0002; // AE, IE
        let bytes =
            unsafe { core::slice::from_raw_parts_mut(window.as_mut_ptr() as *mut u8, 0x10) };
        let hba = UserDevice {
            kind: AHCI,
            ..device(Transport::Pci, bytes)
        };
        // Plain memory keeps the bit: the reset is not taken as done.
        assert!(!hba.reset());
        assert_eq!(window[AHCI_GHC / 4], 0x8000_0003);
    }

    #[test]
    fn a_pci_function_of_another_kind_is_not_reset() {
        let mut window = [0xffu8; E1000E_RESET_SPAN];
        assert!(!device(Transport::Pci, &mut window).reset());
        assert!(window.iter().all(|b| *b == 0xff));
        let mut short = [0xffu8; E1000E_TCTL];
        let card = UserDevice {
            kind: E1000E,
            ..device(Transport::Pci, &mut short)
        };
        assert!(!card.reset());
        assert!(short.iter().all(|b| *b == 0xff));
    }

    #[test]
    fn a_device_on_ports_is_reached_only_inside_its_window() {
        let ports = UserDevice {
            kind: I8042,
            index: 0,
            transport: Transport::Ports,
            paddr: I8042_PORT,
            size: I8042_SPAN,
            irq: Some(I8042_KBD_IRQ),
            vaddr: None,
            claimed: AtomicBool::new(false),
        };
        assert_eq!(ports.port(0, 1), Some(0x60));
        assert_eq!(ports.port(4, 1), Some(0x64));
        assert_eq!(ports.port(5, 1), None, "past the window");
        assert_eq!(ports.port(4, 2), None, "runs past it");
        assert_eq!(ports.port(0, 3), None, "no such width");
        assert_eq!(ports.port(usize::MAX, 1), None);
        let mut window = [0u8; 8];
        let memory = device(Transport::VirtioMmio, &mut window);
        assert_eq!(memory.port(0, 1), None, "memory is not ports");
        // A kind of ports this does not know how to stop.
        let other = UserDevice {
            kind: NVME,
            ..ports
        };
        assert!(!other.reset());
    }

    #[test]
    fn a_device_that_cannot_be_reached_is_not_reset() {
        let mut window = [0u8; 0x200];
        let unmapped = UserDevice {
            vaddr: None,
            ..device(Transport::VirtioMmio, &mut window)
        };
        assert!(!unmapped.reset());
        // A window that ends before the register: nothing is written.
        let mut short = [0xffu8; MMIO_STATUS];
        assert!(!device(Transport::VirtioMmio, &mut short).reset());
        assert!(short.iter().all(|b| *b == 0xff));
        let transport = Transport::VirtioPci {
            common: 0x1f0,
            notify: 0,
            isr: 0,
            device: 0,
            notify_multiplier: 0,
        };
        assert!(!device(transport, &mut window).reset());
    }

    #[test]
    fn the_first_network_card_is_left_to_userspace_without_a_word() {
        // Nothing said: the card, and it was not named.
        assert_eq!(wanted(None, true), [((VIRTIO_NET, 0), false)]);
        assert_eq!(wanted(Some(""), true), [((VIRTIO_NET, 0), false)]);
        // A disk named: the disk, and the card as well.
        assert_eq!(
            wanted(Some("virtio-blk@1"), true),
            [((VIRTIO_BLK, 1), true), ((VIRTIO_NET, 0), false)]
        );
        // A card named, the first or another: that one and no other.
        assert_eq!(
            wanted(Some("virtio-net@0"), true),
            [((VIRTIO_NET, 0), true)]
        );
        assert_eq!(
            wanted(Some("virtio-net@1,virtio-blk@0"), true),
            [((VIRTIO_NET, 1), true), ((VIRTIO_BLK, 0), true)]
        );
        // A boot that wants no card left by itself has what it names.
        assert_eq!(wanted(None, false), []);
        assert_eq!(
            wanted(Some("virtio-blk@1"), false),
            [((VIRTIO_BLK, 1), true)]
        );
        assert_eq!(
            wanted(Some("virtio-net@0"), false),
            [((VIRTIO_NET, 0), true)]
        );
        // An item that names no device changes nothing.
        assert_eq!(wanted(Some("floppy@0"), true), [((VIRTIO_NET, 0), false)]);
    }

    // The global state is set once per process, so one test walks it.
    #[test]
    fn every_disk_and_only_the_wanted_card_are_reserved() {
        set_reservation(Some("virtio-net@0"), true);
        let base = SEEN[0].load(Ordering::Acquire) + SEEN[1].load(Ordering::Acquire);
        // Other tests may not count devices, so the index is absolute only
        // when this test starts from zero.
        if base != 0 {
            return;
        }
        let wired = |irq| Wiring {
            irq: Some(irq),
            vaddr: None,
        };
        // Every disk, named or not: the kernel has no disk driver.
        for (n, paddr) in [0x1000, 0x2000, 0x3000].iter().copied().enumerate() {
            assert!(reserve_virtio_blk(
                Transport::VirtioMmio,
                paddr,
                0x200,
                wired(n + 1)
            ));
        }
        let dev = find(VIRTIO_BLK, 1).unwrap();
        assert_eq!((dev.paddr, dev.size, dev.irq), (0x2000, 0x200, Some(2)));
        assert!(find(VIRTIO_BLK, 0).is_some());
        assert!(find(VIRTIO_BLK, 2).is_some());
        assert!(find(VIRTIO_BLK, 3).is_none());
        // Each kind counts its own devices: the first card is #0 whatever
        // number of disks came before it.
        assert!(reserve_virtio_net(
            Transport::VirtioMmio,
            0x4000,
            0x200,
            wired(4)
        ));
        assert!(!reserve_virtio_net(
            Transport::VirtioMmio,
            0x5000,
            0x200,
            wired(5)
        ));
        let card = find(VIRTIO_NET, 0).unwrap();
        assert_eq!((card.paddr, card.irq), (0x4000, Some(4)));
        assert!(find(VIRTIO_NET, 1).is_none());
    }
}
