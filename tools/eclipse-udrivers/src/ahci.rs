//! An AHCI (SATA) controller, each disk on its ports served to the kernel.
//!
//! The kernel leaves every AHCI controller to userspace, having no driver
//! of its own for one (`udriver=ahci@N` names it); it neither brings it up nor so much as resets it until the
//! process that claimed it dies (`zcore_drivers::user`, which resets the
//! HBA then). What the kernel sees of each disk is what a virtio disk
//! served from userspace gives it: the same messages on the same FIFO and
//! a window of the driver's DMA memory that the controller reads and
//! writes directly ([`crate::blk`]). Its node is an `sdX`.
//!
//! One command at a time on each port, in slot 0, with one PRD for the
//! data: the window is physically contiguous, and a request fits one
//! command (READ/WRITE DMA EXT move up to 65536 sectors). 512-byte
//! sectors and 48-bit addressing only, which is every SATA disk QEMU
//! makes. A disk is one thread of the driver; the controller's interrupt,
//! when the claim gives one, is another, which wakes the port it is for;
//! with a single disk its thread sleeps on the interrupt itself.

use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::blk::{Backend, Io, DATA_MAX, SECTOR};
use crate::claim;
use crate::hybrid::Hybrid;
use crate::zx::{Handle, Status};
use crate::{now_ns, PAGE};

// HBA registers, by byte offset (AHCI 1.3.1 §3.1).
const CAP: usize = 0x00;
const GHC: usize = 0x04;
const IS: usize = 0x08;
const PI: usize = 0x0c;
const GHC_AE: u32 = 1 << 31;
const GHC_IE: u32 = 1 << 1;
const CAP_S64A: u32 = 1 << 31;

// Port registers, by byte offset from the port's (§3.3).
const PORTS: usize = 0x100;
const PORT_SIZE: usize = 0x80;
const P_CLB: usize = 0x00;
const P_FB: usize = 0x08;
const P_IS: usize = 0x10;
const P_IE: usize = 0x14;
const P_CMD: usize = 0x18;
const P_TFD: usize = 0x20;
const P_SIG: usize = 0x24;
const P_SSTS: usize = 0x28;
const P_SCTL: usize = 0x2c;
const P_SERR: usize = 0x30;
const P_CI: usize = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const TFD_ERR: u32 = 1 << 0;
const TFD_DRQ: u32 = 1 << 3;
const TFD_BSY: u32 = 1 << 7;
/// `PxIS`/`PxIE`: a device-to-host register FIS came (the end of a
/// command).
const IS_DHRS: u32 = 1 << 0;
/// What `PxIS` says of an error: task file, host bus data and fatal,
/// interface fatal and non-fatal, overflow.
const IS_ERRORS: u32 = (1 << 30) | (1 << 29) | (1 << 28) | (1 << 27) | (1 << 26) | (1 << 24);
/// `PxSIG` of an ATA disk (an ATAPI drive says `0xeb140101`).
pub const SIG_ATA: u32 = 0x0000_0101;
const SIG_ATAPI: u32 = 0xeb14_0101;

// ATA commands (ACS-3).
const ATA_IDENTIFY: u8 = 0xec;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_EXT: u8 = 0xea;

/// The DMA area of one port, one contiguous VMO: the command list (1 KiB,
/// 32 headers of which slot 0 is used), the received FIS (256 bytes) and
/// the command table of slot 0 in the first page; IDENTIFY's answer in the
/// second; then the data window.
pub const DMA_CLB: usize = 0;
pub const DMA_FB: usize = 0x400;
pub const DMA_CTBA: usize = 0x800;
pub const DMA_IDENTIFY: usize = PAGE;
pub const DMA_DATA: usize = 2 * PAGE;
pub const DMA_SIZE: usize = DMA_DATA + DATA_MAX;
/// The PRD table of a command table starts here (§4.2.3).
const CT_PRDT: usize = 0x80;
const _: () = assert!(DATA_MAX / SECTOR <= 65536);
const _: () = assert!(DMA_CTBA + CT_PRDT + 16 <= PAGE);
const _: () = assert!(DMA_CTBA >= DMA_FB + 256);
const _: () = assert!(DMA_FB >= DMA_CLB + 32 * 32);

/// How long a wait looks at the port before it sleeps on its wake.
const DEVICE_LOOK_NS: u64 = 200_000;
/// How long a port has to stop, start, or answer IDENTIFY.
const SETTLE_NS: u64 = 1_000_000_000;
/// How long a command may take before the disk is given up on.
const COMMAND_NS: u64 = 30_000_000_000;

/// The controller's registers.
#[derive(Clone, Copy)]
pub struct Regs(*mut u8);
// SAFETY: MMIO, touched with volatile accesses; each port's registers are
// only written by that port's thread, and `IS` by the interrupt's (by the
// port's, when it is the only one: a bit of its own).
unsafe impl Send for Regs {}
unsafe impl Sync for Regs {}

impl Regs {
    /// # Safety
    /// `base` is the claimed ABAR, mapped for good.
    pub unsafe fn new(base: *mut u8) -> Regs {
        Regs(base)
    }

    fn read(&self, reg: usize) -> u32 {
        // SAFETY: a register inside the claimed ABAR.
        unsafe { ptr::read_volatile(self.0.add(reg).cast::<u32>()) }
    }

    fn write(&self, reg: usize, v: u32) {
        // SAFETY: as above.
        unsafe { ptr::write_volatile(self.0.add(reg).cast::<u32>(), v) }
    }

    fn port(&self, p: usize, reg: usize) -> usize {
        PORTS + p * PORT_SIZE + reg
    }

    /// AHCI mode on, and whether the controller addresses 64 bits.
    pub fn enable(&self) -> bool {
        self.write(GHC, self.read(GHC) | GHC_AE);
        self.read(CAP) & CAP_S64A != 0
    }

    /// The ports with an ATA disk on them, linked and awake. The ports
    /// whose link is not up are reset first, all at once (COMRESET,
    /// §10.4.2), and given a moment: after the kernel reset the HBA, for a
    /// driver that died, every link is down until that. A port that sees
    /// no device (`DET` 0) is not waited for past that moment; one that
    /// sees one (`DET` 1) gets a second to talk.
    pub fn disks(&self) -> Vec<usize> {
        let pi = self.read(PI);
        let det = |p: usize| self.read(self.port(p, P_SSTS)) & 0xf;
        let down: Vec<usize> = (0..32)
            .filter(|p| pi & (1 << p) != 0 && det(*p) != 3)
            .collect();
        if !down.is_empty() {
            for &p in &down {
                let sctl = self.port(p, P_SCTL);
                self.write(sctl, (self.read(sctl) & !0xf) | 1);
            }
            std::thread::sleep(Duration::from_millis(2));
            for &p in &down {
                let sctl = self.port(p, P_SCTL);
                self.write(sctl, self.read(sctl) & !0xf);
            }
            std::thread::sleep(Duration::from_millis(50));
            let start = now_ns();
            while down.iter().any(|&p| det(p) == 1) && now_ns() - start < SETTLE_NS {
                std::thread::sleep(Duration::from_millis(1));
            }
            for &p in &down {
                self.write(self.port(p, P_SERR), u32::MAX);
            }
        }
        (0..32)
            .filter(|p| pi & (1 << p) != 0)
            .filter(|&p| {
                let ssts = self.read(self.port(p, P_SSTS));
                // The signature is only known once the port has received
                // the device's first FIS, which after a reset is when
                // its driver starts receiving: an ATAPI drive is left
                // out here when it is known, and by `Port::new` when not.
                ssts & 0xf == 3
                    && (ssts >> 8) & 0xf == 1
                    && self.read(self.port(p, P_SIG)) != SIG_ATAPI
            })
            .collect()
    }

    /// Interrupts from the HBA on, once each port's `PxIE` says which.
    pub fn interrupts_on(&self) {
        self.write(IS, u32::MAX);
        self.write(GHC, self.read(GHC) | GHC_IE);
    }
}

/// What wakes a port's thread: the interrupt's thread counts each
/// interrupt for the port and keeps its error bits.
#[derive(Default)]
pub struct Wake {
    count: Mutex<u64>,
    cond: Condvar,
    errors: AtomicU32,
}

impl Wake {
    fn ring(&self, pis: u32) {
        if pis & IS_ERRORS != 0 {
            self.errors.fetch_or(pis & IS_ERRORS, Ordering::AcqRel);
        }
        *self.count.lock().unwrap() += 1;
        self.cond.notify_all();
    }

    fn seen(&self) -> u64 {
        *self.count.lock().unwrap()
    }

    /// Asleep until the count passes `seen`, or a moment has gone by.
    fn wait(&self, seen: u64) {
        let count = self.count.lock().unwrap();
        let _ = self
            .cond
            .wait_timeout_while(count, Duration::from_millis(10), |c| *c == seen);
    }
}

/// How a port's thread learns that its command is done.
pub enum Waker {
    /// No interrupt: it looks, and yields.
    Poll,
    /// Woken by the interrupt's thread, which serves several ports.
    Shared(Arc<Wake>),
    /// Asleep on the controller's interrupt itself, the only port on it:
    /// no other thread, no hop between two.
    Direct(Handle),
}

/// The controller's interrupt, for every port at once: each that raised
/// it has its `PxIS` cleared and its thread woken. Until the wait fails.
pub fn interrupts(regs: Regs, irq: Handle, wakes: Vec<(usize, Arc<Wake>)>) -> Status {
    loop {
        if let Err(e) = claim::interrupt_wait(irq) {
            return e;
        }
        let is = regs.read(IS);
        for (p, wake) in &wakes {
            if is & (1 << p) != 0 {
                let at = regs.port(*p, P_IS);
                let pis = regs.read(at);
                regs.write(at, pis);
                wake.ring(pis);
            }
        }
        regs.write(IS, is);
    }
}

/// A register FIS, host to device, for `command` on `count` sectors from
/// `lba` (SATA 3.x §10.5.5): the 20 bytes of a command table's CFIS.
pub fn h2d_fis(command: u8, lba: u64, count: u32) -> [u8; 20] {
    let mut f = [0u8; 20];
    f[0] = 0x27;
    f[1] = 0x80; // a command, not a control update
    f[2] = command;
    f[4] = lba as u8;
    f[5] = (lba >> 8) as u8;
    f[6] = (lba >> 16) as u8;
    f[7] = 0x40; // LBA addressing
    f[8] = (lba >> 24) as u8;
    f[9] = (lba >> 32) as u8;
    f[10] = (lba >> 40) as u8;
    // 65536 sectors is a count of 0.
    f[12] = count as u8;
    f[13] = (count >> 8) as u8;
    f
}

/// Word 0 of a command header (§4.2.2): the FIS length in dwords, the
/// direction, and how many PRDs.
pub fn header_dw0(write: bool, prds: u16) -> u32 {
    5 | (write as u32) << 6 | (prds as u32) << 16
}

/// What IDENTIFY DEVICE says of the disk that matters here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// Sectors, by 48-bit addressing.
    pub sectors: u64,
    /// Whether it takes FLUSH CACHE EXT.
    pub flush: bool,
    /// Bytes per logical sector.
    pub sector_size: u32,
    pub lba48: bool,
}

/// Read IDENTIFY DEVICE's 256 words (ACS-3 §7.12.7).
pub fn identity(w: &[u16; 256]) -> Identity {
    let lba48 = w[83] & (1 << 10) != 0;
    let sectors = (0..4).fold(0u64, |acc, i| acc | (w[100 + i] as u64) << (16 * i));
    // Word 106: valid (bit 14 set, 15 clear), and bit 12 if the logical
    // sector is longer than 256 words, its length in words 117-118.
    let sector_size = if w[106] & 0xc000 == 0x4000 && w[106] & (1 << 12) != 0 {
        2 * (w[117] as u32 | (w[118] as u32) << 16)
    } else {
        SECTOR as u32
    };
    Identity {
        sectors,
        flush: w[83] & (1 << 13) != 0,
        sector_size,
        lba48,
    }
}

/// One port with a disk: its registers, its DMA memory and its wake.
pub struct Port {
    regs: Regs,
    p: usize,
    dma: *mut u8,
    daddr: u64,
    wake: Waker,
    look: Hybrid,
    identity: Identity,
}

// SAFETY: a port is driven by one thread; its DMA memory is its own.
unsafe impl Send for Port {}

fn until(what: &str, ns: u64, mut ok: impl FnMut() -> bool) -> Result<(), String> {
    let start = now_ns();
    while !ok() {
        if now_ns() - start > ns {
            return Err(format!("the port never {what}"));
        }
        std::thread::yield_now();
    }
    Ok(())
}

impl Port {
    /// Bring port `p` up on its DMA memory, and ask its disk what it is.
    ///
    /// # Safety
    /// `dma` is [`DMA_SIZE`] zeroed bytes of DMA memory the controller
    /// sees at `daddr`, mapped for as long as the `Port` lives and touched
    /// by nobody else but the controller and, in the window, the kernel.
    pub unsafe fn new(
        regs: Regs,
        p: usize,
        dma: *mut u8,
        daddr: u64,
        wake: Waker,
    ) -> Result<Port, String> {
        let mut port = Port {
            regs,
            p,
            dma,
            daddr,
            wake,
            look: Hybrid::new(true),
            identity: Identity {
                sectors: 0,
                flush: false,
                sector_size: 0,
                lba48: false,
            },
        };
        port.stop()?;
        let r = |reg| regs.port(p, reg);
        regs.write(r(P_CLB), (daddr + DMA_CLB as u64) as u32);
        regs.write(r(P_CLB) + 4, ((daddr + DMA_CLB as u64) >> 32) as u32);
        regs.write(r(P_FB), (daddr + DMA_FB as u64) as u32);
        regs.write(r(P_FB) + 4, ((daddr + DMA_FB as u64) >> 32) as u32);
        port.start()?;
        let sig = regs.read(r(P_SIG));
        if sig != SIG_ATA {
            let _ = port.stop();
            return Err(format!("port {p}: not an ATA disk (signature {sig:#x})"));
        }
        regs.write(
            r(P_IE),
            if !matches!(port.wake, Waker::Poll) {
                IS_DHRS | IS_ERRORS
            } else {
                0
            },
        );
        // IDENTIFY DEVICE: PIO in, 512 bytes into its page.
        if !port.issue(ATA_IDENTIFY, 0, 0, Some((DMA_IDENTIFY, 512)), false) {
            return Err(format!("port {p}: IDENTIFY DEVICE failed"));
        }
        let mut words = [0u16; 256];
        ptr::copy_nonoverlapping(dma.add(DMA_IDENTIFY).cast::<u16>(), words.as_mut_ptr(), 256);
        port.identity = identity(&words);
        if !port.identity.lba48 {
            return Err(format!("port {p}: the disk has no 48-bit addressing"));
        }
        if port.identity.sector_size != SECTOR as u32 {
            return Err(format!(
                "port {p}: the disk's sectors are {} bytes; this driver serves 512",
                port.identity.sector_size
            ));
        }
        Ok(port)
    }

    /// The disk as IDENTIFY described it.
    pub fn identity(&self) -> Identity {
        self.identity
    }

    fn reg(&self, reg: usize) -> usize {
        self.regs.port(self.p, reg)
    }

    /// Stop the command engine and the FIS receive (§10.1.2).
    fn stop(&self) -> Result<(), String> {
        let cmd = self.reg(P_CMD);
        self.regs.write(cmd, self.regs.read(cmd) & !CMD_ST);
        until("stopped its commands", SETTLE_NS, || {
            self.regs.read(cmd) & CMD_CR == 0
        })?;
        self.regs.write(cmd, self.regs.read(cmd) & !CMD_FRE);
        until("stopped receiving", SETTLE_NS, || {
            self.regs.read(cmd) & CMD_FR == 0
        })
    }

    /// Receive FISes, clear what the port says went wrong, and start the
    /// command engine once the disk is idle.
    fn start(&self) -> Result<(), String> {
        let cmd = self.reg(P_CMD);
        self.regs.write(cmd, self.regs.read(cmd) | CMD_FRE);
        self.regs.write(self.reg(P_SERR), u32::MAX);
        self.regs.write(self.reg(P_IS), u32::MAX);
        until("came idle", SETTLE_NS, || {
            self.regs.read(self.reg(P_TFD)) & (TFD_BSY | TFD_DRQ) == 0
        })?;
        self.regs.write(cmd, self.regs.read(cmd) | CMD_ST);
        Ok(())
    }

    /// After an error: the engine stopped and started again, so the next
    /// command finds the port as new.
    fn recover(&self) {
        let _ = self.stop().and_then(|()| self.start());
        if let Waker::Direct(_) = self.wake {
            self.regs.write(IS, 1 << self.p);
        }
        if let Waker::Shared(wake) = &self.wake {
            wake.errors.store(0, Ordering::Release);
        }
    }

    /// Put one command in slot 0 and wait for it: `data` is where in the
    /// DMA area its bytes are, and how many. Whether the disk did it.
    fn issue(
        &mut self,
        command: u8,
        lba: u64,
        count: u32,
        data: Option<(usize, usize)>,
        write: bool,
    ) -> bool {
        let fis = h2d_fis(command, lba, count);
        // SAFETY: the command table and header of slot 0, in the port's
        // first page; aligned for the words written.
        unsafe {
            let ct = self.dma.add(DMA_CTBA);
            ptr::write_bytes(ct, 0, CT_PRDT + 16);
            ptr::copy_nonoverlapping(fis.as_ptr(), ct, fis.len());
            let prds = match data {
                Some((off, len)) => {
                    let prd = ct.add(CT_PRDT).cast::<u32>();
                    let dba = self.daddr + off as u64;
                    ptr::write_volatile(prd, dba as u32);
                    ptr::write_volatile(prd.add(1), (dba >> 32) as u32);
                    ptr::write_volatile(prd.add(2), 0);
                    ptr::write_volatile(prd.add(3), (len as u32 - 1) | 1 << 31);
                    1
                }
                None => 0,
            };
            let h = self.dma.add(DMA_CLB).cast::<u32>();
            let ctba = self.daddr + DMA_CTBA as u64;
            ptr::write_volatile(h, header_dw0(write, prds));
            ptr::write_volatile(h.add(1), 0);
            ptr::write_volatile(h.add(2), ctba as u32);
            ptr::write_volatile(h.add(3), (ctba >> 32) as u32);
        }
        core::sync::atomic::fence(Ordering::SeqCst);
        let seen = match &self.wake {
            Waker::Shared(w) => Some(w.seen()),
            _ => None,
        };
        self.regs.write(self.reg(P_CI), 1);
        let ok = self.wait(
            seen,
            command == ATA_READ_DMA_EXT || command == ATA_WRITE_DMA_EXT,
        );
        if !ok {
            self.recover();
        }
        ok
    }

    /// Wait for slot 0 to be done: a look first when looks have been
    /// paying, then asleep on the port's wake (or yielding, with no
    /// interrupt). Whether it ended well.
    fn wait(&mut self, mut seen: Option<u64>, lookable: bool) -> bool {
        let ci = self.reg(P_CI);
        let tfd = self.reg(P_TFD);
        let pis = self.reg(P_IS);
        let started = now_ns();
        let failed = |port: &Port| {
            let errors = match &port.wake {
                Waker::Shared(w) => w.errors.load(Ordering::Acquire),
                _ => port.regs.read(pis) & IS_ERRORS,
            };
            errors != 0
        };
        let mut looking = lookable && self.look.try_look();
        let mut turns = 1u32;
        loop {
            if self.regs.read(ci) & 1 == 0 {
                break;
            }
            if failed(self) {
                return false;
            }
            if now_ns() - started > COMMAND_NS {
                return false;
            }
            if looking {
                if turns.is_multiple_of(1024) && now_ns() - started >= DEVICE_LOOK_NS {
                    self.look.done(false);
                    looking = false;
                }
                turns += 1;
                core::hint::spin_loop();
                continue;
            }
            match (&self.wake, seen) {
                (Waker::Shared(wake), Some(s)) => {
                    wake.wait(s);
                    seen = Some(wake.seen());
                }
                // Only a DMA command is sure to end in a D2H FIS, so an
                // interrupt: IDENTIFY ends in a PIO Setup one and FLUSH
                // is rare, and those two are watched.
                (Waker::Direct(irq), _) if lookable => {
                    if claim::interrupt_wait(*irq).is_err() {
                        return false;
                    }
                }
                _ => std::thread::yield_now(),
            }
        }
        if looking {
            self.look.done(true);
        }
        if !matches!(self.wake, Waker::Shared(_)) {
            // Nobody else clears it; with the interrupt, the controller's
            // bit too, or the line stays up.
            let v = self.regs.read(pis);
            self.regs.write(pis, v);
            if matches!(self.wake, Waker::Direct(_)) {
                self.regs.write(IS, 1 << self.p);
            }
        }
        self.regs.read(tfd) & TFD_ERR == 0 && !failed(self)
    }
}

impl Backend for Port {
    fn capacity(&self) -> u64 {
        self.identity.sectors
    }

    fn has_flush(&self) -> bool {
        self.identity.flush
    }

    fn run(&mut self, ios: &[Io], done: &mut [bool]) -> Result<(), Status> {
        for (io, done) in ios.iter().zip(done.iter_mut()) {
            *done = if io.is_flush() {
                self.issue(ATA_FLUSH_EXT, 0, 0, None, false)
            } else {
                let sectors = (io.len() / SECTOR) as u32;
                let (command, write) = if io.is_read() {
                    (ATA_READ_DMA_EXT, false)
                } else {
                    (ATA_WRITE_DMA_EXT, true)
                };
                self.issue(
                    command,
                    io.sector(),
                    sectors,
                    Some((DMA_DATA + io.off(), io.len())),
                    write,
                )
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_fis_carries_the_command_the_lba_and_the_count() {
        let f = h2d_fis(ATA_WRITE_DMA_EXT, 0x0000_0605_0403_0201, 8);
        assert_eq!(&f[0..4], &[0x27, 0x80, ATA_WRITE_DMA_EXT, 0]);
        assert_eq!(&f[4..7], &[0x01, 0x02, 0x03]);
        assert_eq!(f[7], 0x40, "LBA mode");
        assert_eq!(&f[8..11], &[0x04, 0x05, 0x06]);
        assert_eq!((f[12], f[13]), (8, 0));
        // The most a command moves: 65536 sectors, counted as 0.
        let f = h2d_fis(ATA_READ_DMA_EXT, 0, 65536);
        assert_eq!((f[12], f[13]), (0, 0));
        let f = h2d_fis(ATA_READ_DMA_EXT, 0, 2048);
        assert_eq!((f[12], f[13]), (0, 8));
    }

    #[test]
    fn a_header_says_five_dwords_of_fis_the_direction_and_the_prds() {
        assert_eq!(header_dw0(false, 1), 5 | 1 << 16);
        assert_eq!(header_dw0(true, 1), 5 | 1 << 6 | 1 << 16);
        assert_eq!(header_dw0(false, 0), 5, "a flush has no data");
    }

    #[test]
    fn identify_gives_the_size_the_flush_and_the_sector_length() {
        let mut w = [0u16; 256];
        w[83] = (1 << 10) | (1 << 13);
        w[100] = 0x0000;
        w[101] = 0x0001; // 65536 sectors
        let id = identity(&w);
        assert_eq!(id.sectors, 65536);
        assert!(id.flush && id.lba48);
        assert_eq!(id.sector_size, 512);
        // A disk of 4 KiB logical sectors says so in word 106.
        w[106] = 0x4000 | (1 << 12);
        w[117] = 2048;
        assert_eq!(identity(&w).sector_size, 4096);
        // Word 106 not valid: 512, whatever it says.
        w[106] = 1 << 12;
        assert_eq!(identity(&w).sector_size, 512);
        w[83] = 0;
        assert!(!identity(&w).lba48 && !identity(&w).flush);
    }

    #[test]
    fn the_dma_area_keeps_each_structure_aligned() {
        assert_eq!(DMA_CLB % 1024, 0);
        assert_eq!(DMA_FB % 256, 0);
        assert_eq!(DMA_CTBA % 128, 0);
        assert_eq!(DMA_DATA % PAGE, 0);
    }
}
