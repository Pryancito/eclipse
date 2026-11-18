//! An NVMe controller, served to the kernel as a disk.
//!
//! The boot leaves the controller to userspace with `udriver=nvme@N`; the
//! kernel then neither brings it up nor so much as resets it until the
//! process that claimed it dies (`zcore_drivers::user`, which clears
//! `CC.EN` then). What the kernel sees is what a virtio disk served from
//! userspace gives it: the same messages on the same FIFO and a window of
//! the driver's DMA memory that the controller reads and writes directly
//! ([`crate::blk`]). Its node is an `nvmeNn1`.
//!
//! One namespace (the first), one I/O queue pair, 512-byte blocks: QEMU's
//! `nvme` and most disks as they come. Everything one read of the FIFO
//! brought goes on the queue at once, with one doorbell, as the virtio
//! disk does; a request longer than the controller takes in one command
//! (`MDTS`) is cut in several.
//!
//! The admin queue is only used while bringing the controller up, and is
//! polled. The I/O completion queue raises interrupt vector 0 when the
//! claim gives an interrupt; without one, it is polled too.

use core::ptr;

use crate::blk::{Backend, Io, DATA_MAX, SECTOR};
use crate::claim;
use crate::hybrid::Hybrid;
use crate::zx::{Handle, Status};
use crate::{now_ns, PAGE};

// Controller registers, by byte offset (NVMe 1.4 §3.1).
const CAP: usize = 0x00;
const INTMS: usize = 0x0c;
const CC: usize = 0x14;
const CSTS: usize = 0x1c;
const AQA: usize = 0x24;
const ASQ: usize = 0x28;
const ACQ: usize = 0x30;
const DOORBELLS: usize = 0x1000;

const CC_EN: u32 = 1;
/// `IOSQES` 6 (64-byte entries) and `IOCQES` 4 (16-byte entries).
const CC_ENTRY_SIZES: u32 = (6 << 16) | (4 << 20);
const CSTS_RDY: u32 = 1;
const CSTS_CFS: u32 = 2;

// Admin commands (§5).
const ADMIN_CREATE_SQ: u8 = 0x01;
const ADMIN_CREATE_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
// NVM commands (§6).
const NVM_FLUSH: u8 = 0x00;
const NVM_WRITE: u8 = 0x01;
const NVM_READ: u8 = 0x02;

/// Entries of the admin queues, and of the I/O ones.
const ADMIN_DEPTH: u16 = 16;
const IO_DEPTH: u16 = 64;
/// Commands on the I/O queue at once: fewer than its entries, so it is
/// never full.
pub const MAX_CMDS: usize = 32;
const SQE: usize = 64;
const CQE: usize = 16;
const NSID: u32 = 1;

/// The DMA area, one contiguous VMO: the four queues a page each, a page
/// for what Identify answers, a page of PRP list per command, then the
/// data window ([`DATA_MAX`] bytes) the kernel reads and writes.
pub const DMA_ASQ: usize = 0;
pub const DMA_ACQ: usize = PAGE;
pub const DMA_IOSQ: usize = 2 * PAGE;
pub const DMA_IOCQ: usize = 3 * PAGE;
pub const DMA_IDENTIFY: usize = 4 * PAGE;
pub const DMA_PRP: usize = 5 * PAGE;
pub const DMA_DATA: usize = DMA_PRP + MAX_CMDS * PAGE;
pub const DMA_SIZE: usize = DMA_DATA + DATA_MAX;
const _: () = assert!(ADMIN_DEPTH as usize * SQE <= PAGE);
const _: () = assert!(IO_DEPTH as usize * SQE <= PAGE);
const _: () = assert!(MAX_CMDS < IO_DEPTH as usize);
const _: () = assert!(DMA_IDENTIFY < DMA_PRP);

/// How long a wait looks at the completion queue before it sleeps on the
/// interrupt (see [`Hybrid`]).
const DEVICE_LOOK_NS: u64 = 200_000;
/// How long the controller has to become ready, or stop being so.
const READY_NS: u64 = 5_000_000_000;

/// One submission queue entry, as the controller reads it (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Command {
    pub opcode: u8,
    pub cid: u16,
    pub nsid: u32,
    pub prp1: u64,
    pub prp2: u64,
    /// Command dwords 10 to 15.
    pub cdw: [u32; 6],
}

impl Command {
    /// The 64 bytes of the entry.
    pub fn encode(&self) -> [u32; 16] {
        let mut d = [0u32; 16];
        d[0] = self.opcode as u32 | (self.cid as u32) << 16;
        d[1] = self.nsid;
        d[6] = self.prp1 as u32;
        d[7] = (self.prp1 >> 32) as u32;
        d[8] = self.prp2 as u32;
        d[9] = (self.prp2 >> 32) as u32;
        d[10..16].copy_from_slice(&self.cdw);
        d
    }

    /// A read or a write of `blocks` 512-byte blocks from `lba`.
    pub fn rw(write: bool, lba: u64, blocks: u32, prp: (u64, u64)) -> Command {
        Command {
            opcode: if write { NVM_WRITE } else { NVM_READ },
            nsid: NSID,
            prp1: prp.0,
            prp2: prp.1,
            cdw: [lba as u32, (lba >> 32) as u32, blocks - 1, 0, 0, 0],
            ..Command::default()
        }
    }
}

/// One completion queue entry (§4.6): the command it answers, whether it
/// went well, and the phase bit that says the entry is new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    pub cid: u16,
    pub sq_head: u16,
    pub phase: bool,
    /// Status code and type; 0 is success.
    pub status: u16,
}

impl Completion {
    pub fn decode(d: [u32; 4]) -> Completion {
        Completion {
            sq_head: d[2] as u16,
            cid: d[3] as u16,
            phase: d[3] & (1 << 16) != 0,
            status: ((d[3] >> 17) & 0x7fff) as u16,
        }
    }
}

/// The PRP entries of a transfer of `len` bytes at device address `addr`
/// (§4.3): the first, which may start anywhere in its page, and then
/// either nothing, the second page, or the address of `list` -- filled
/// here, at device address `list_addr` -- with every page after the first.
/// `None` when it does not fit one list page.
pub fn prps(addr: u64, len: usize, list: &mut [u64], list_addr: u64) -> Option<(u64, u64)> {
    let page = PAGE as u64;
    let first_end = (addr / page + 1) * page;
    let end = addr + len as u64;
    if end <= first_end {
        return Some((addr, 0));
    }
    let more = (end - first_end).div_ceil(page) as usize;
    if more == 1 {
        return Some((addr, first_end));
    }
    if more > list.len() {
        return None;
    }
    for (i, entry) in list[..more].iter_mut().enumerate() {
        *entry = first_end + i as u64 * page;
    }
    Some((addr, list_addr))
}

/// A request cut in what one command can move: `(sector, bytes, offset)`
/// of each piece, in order.
pub fn pieces(io: &Io, max: usize) -> impl Iterator<Item = (u64, usize, usize)> + '_ {
    let n = if io.is_flush() {
        1
    } else {
        io.len().div_ceil(max)
    };
    (0..n).map(move |k| {
        let at = k * max;
        let len = io.len().saturating_sub(at).min(max);
        (io.sector() + (at / SECTOR) as u64, len, io.off() + at)
    })
}

/// The controller's registers.
struct Regs(*mut u8);

impl Regs {
    fn read32(&self, reg: usize) -> u32 {
        // SAFETY: a register inside the claimed BAR, mapped for good.
        unsafe { ptr::read_volatile(self.0.add(reg).cast::<u32>()) }
    }

    fn write32(&self, reg: usize, v: u32) {
        // SAFETY: as above.
        unsafe { ptr::write_volatile(self.0.add(reg).cast::<u32>(), v) }
    }

    fn read64(&self, reg: usize) -> u64 {
        self.read32(reg) as u64 | (self.read32(reg + 4) as u64) << 32
    }

    fn write64(&self, reg: usize, v: u64) {
        self.write32(reg, v as u32);
        self.write32(reg + 4, (v >> 32) as u32);
    }
}

/// One queue pair: a submission queue and the completion queue it
/// answers on.
struct Pair {
    sq: *mut u8,
    cq: *mut u8,
    depth: u16,
    tail: u16,
    head: u16,
    phase: bool,
    sq_bell: usize,
    cq_bell: usize,
}

impl Pair {
    fn new(sq: *mut u8, cq: *mut u8, depth: u16, qid: usize, stride: usize) -> Pair {
        Pair {
            sq,
            cq,
            depth,
            tail: 0,
            head: 0,
            phase: true,
            sq_bell: DOORBELLS + 2 * qid * stride,
            cq_bell: DOORBELLS + (2 * qid + 1) * stride,
        }
    }

    /// Put `cmd` in the next entry. Nothing is the controller's until
    /// [`ring`](Self::ring).
    fn push(&mut self, cmd: &Command) {
        let d = cmd.encode();
        // SAFETY: entry `tail` of the queue's page.
        unsafe {
            let e = self.sq.add(self.tail as usize * SQE).cast::<u32>();
            for (i, w) in d.iter().enumerate() {
                ptr::write_volatile(e.add(i), *w);
            }
        }
        self.tail = (self.tail + 1) % self.depth;
    }

    fn ring(&self, regs: &Regs) {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        regs.write32(self.sq_bell, self.tail as u32);
    }

    /// The next completion, if the controller has posted one.
    fn peek(&self) -> Option<Completion> {
        // SAFETY: entry `head` of the completion queue's page.
        let d = unsafe {
            let e = self.cq.add(self.head as usize * CQE).cast::<u32>();
            [0, 1, 2, 3].map(|i| ptr::read_volatile(e.add(i)))
        };
        let c = Completion::decode(d);
        (c.phase == self.phase).then_some(c)
    }

    fn pop(&mut self) {
        self.head += 1;
        if self.head == self.depth {
            self.head = 0;
            self.phase = !self.phase;
        }
    }

    /// Tell the controller the completions taken so far are free.
    fn release(&self, regs: &Regs) {
        regs.write32(self.cq_bell, self.head as u32);
    }
}

/// The disk: the controller, its I/O queues and its DMA memory.
pub struct Nvme {
    regs: Regs,
    io: Pair,
    dma: *mut u8,
    daddr: u64,
    interrupt: Option<Handle>,
    look: Hybrid,
    /// Bytes one command moves at most.
    max_xfer: usize,
    capacity: u64,
    has_flush: bool,
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) -> Result<(), String> {
    let start = now_ns();
    while !ok() {
        if now_ns() - start > READY_NS {
            return Err(format!("the controller never {what}"));
        }
        std::thread::yield_now();
    }
    Ok(())
}

impl Nvme {
    /// Bring the controller up: a reset, the admin queues, what it is and
    /// what its first namespace is, and the one I/O queue pair.
    ///
    /// # Safety
    /// `regs` is the claimed BAR, mapped for good; `dma` is [`DMA_SIZE`]
    /// bytes of DMA memory the controller sees at `daddr`, zeroed, mapped
    /// for as long as the `Nvme` lives and touched by nobody else but the
    /// controller and, in the data window, the kernel.
    pub unsafe fn new(
        regs: *mut u8,
        dma: *mut u8,
        daddr: u64,
        interrupt: Option<Handle>,
    ) -> Result<Nvme, String> {
        let regs = Regs(regs);
        let cap = regs.read64(CAP);
        let stride = 4usize << ((cap >> 32) & 0xf);
        let mpsmin = (cap >> 48) & 0xf;
        if mpsmin != 0 {
            return Err(format!(
                "the controller's smallest page is 2^{} bytes",
                12 + mpsmin
            ));
        }
        let mqes = (cap & 0xffff) as u16 + 1;
        if mqes < IO_DEPTH {
            return Err(format!(
                "the controller's queues hold {mqes} entries at most"
            ));
        }
        // Whatever the firmware or a driver before this one left running.
        regs.write32(CC, regs.read32(CC) & !CC_EN);
        wait_until("stopped", || regs.read32(CSTS) & CSTS_RDY == 0)?;
        let aqa = (ADMIN_DEPTH as u32 - 1) << 16 | (ADMIN_DEPTH as u32 - 1);
        regs.write32(AQA, aqa);
        regs.write64(ASQ, daddr + DMA_ASQ as u64);
        regs.write64(ACQ, daddr + DMA_ACQ as u64);
        // Pin-based and single-message MSI interrupts can be masked: none
        // while the admin queue is polled.
        regs.write32(INTMS, u32::MAX);
        regs.write32(CC, CC_ENTRY_SIZES | CC_EN);
        wait_until("became ready", || {
            regs.read32(CSTS) & (CSTS_RDY | CSTS_CFS) != 0
        })?;
        if regs.read32(CSTS) & CSTS_CFS != 0 {
            return Err("the controller failed as it started (CSTS.CFS)".into());
        }
        let mut admin = Pair::new(dma.add(DMA_ASQ), dma.add(DMA_ACQ), ADMIN_DEPTH, 0, stride);
        let identify = daddr + DMA_IDENTIFY as u64;
        let ident = |admin: &mut Pair, cns: u32, nsid: u32| -> Result<[u8; PAGE], String> {
            let cmd = Command {
                opcode: ADMIN_IDENTIFY,
                nsid,
                prp1: identify,
                cdw: [cns, 0, 0, 0, 0, 0],
                ..Command::default()
            };
            admin_call(&regs, admin, &cmd)?;
            let mut page = [0u8; PAGE];
            ptr::copy_nonoverlapping(dma.add(DMA_IDENTIFY), page.as_mut_ptr(), PAGE);
            Ok(page)
        };
        let ctrl = ident(&mut admin, 1, 0)?;
        let mdts = ctrl[77];
        let has_flush = ctrl[525] & 1 != 0;
        let namespaces = u32::from_le_bytes(ctrl[516..520].try_into().unwrap());
        if namespaces < NSID {
            return Err("the controller has no namespace".into());
        }
        let ns = ident(&mut admin, 0, NSID)?;
        let blocks = u64::from_le_bytes(ns[0..8].try_into().unwrap());
        let format = (ns[26] & 0xf) as usize;
        let lbaf = u32::from_le_bytes(ns[128 + 4 * format..132 + 4 * format].try_into().unwrap());
        let lbads = (lbaf >> 16) & 0xff;
        if lbads != 9 {
            return Err(format!(
                "namespace {NSID} has blocks of 2^{lbads} bytes; this driver serves 512"
            ));
        }
        let max_xfer = match mdts {
            0 => DATA_MAX,
            m => (PAGE << m).min(DATA_MAX),
        };
        // The I/O completion queue, with vector 0 when there is an
        // interrupt to sleep on; then its submission queue.
        let ien = interrupt.is_some() as u32;
        let qsize = (IO_DEPTH as u32 - 1) << 16;
        admin_call(
            &regs,
            &mut admin,
            &Command {
                opcode: ADMIN_CREATE_CQ,
                prp1: daddr + DMA_IOCQ as u64,
                cdw: [qsize | 1, (ien << 1) | 1, 0, 0, 0, 0],
                ..Command::default()
            },
        )?;
        admin_call(
            &regs,
            &mut admin,
            &Command {
                opcode: ADMIN_CREATE_SQ,
                prp1: daddr + DMA_IOSQ as u64,
                cdw: [qsize | 1, 1 << 16 | 1, 0, 0, 0, 0],
                ..Command::default()
            },
        )?;
        if interrupt.is_some() {
            // Vector 0, the only one: INTMC.
            regs.write32(INTMS + 4, 1);
        }
        Ok(Nvme {
            io: Pair::new(dma.add(DMA_IOSQ), dma.add(DMA_IOCQ), IO_DEPTH, 1, stride),
            regs,
            dma,
            daddr,
            interrupt,
            look: Hybrid::new(true),
            max_xfer,
            capacity: blocks,
            has_flush,
        })
    }

    /// Bytes one command moves at most.
    pub fn max_xfer(&self) -> usize {
        self.max_xfer
    }

    /// The command for one piece of a request, its PRP list (if it needs
    /// one) in the page of command `slot`.
    fn command(&mut self, io: &Io, piece: (u64, usize, usize), slot: usize) -> Command {
        if io.is_flush() {
            return Command {
                opcode: NVM_FLUSH,
                nsid: NSID,
                ..Command::default()
            };
        }
        let (sector, len, off) = piece;
        let list_off = DMA_PRP + slot * PAGE;
        // SAFETY: the command's own page of the PRP lists.
        let list = unsafe {
            core::slice::from_raw_parts_mut(self.dma.add(list_off).cast::<u64>(), PAGE / 8)
        };
        let addr = self.daddr + (DMA_DATA + off) as u64;
        let prp = prps(addr, len, list, self.daddr + list_off as u64)
            .expect("a piece is at most DATA_MAX, which one list page holds");
        Command::rw(!io.is_read(), sector, (len / SECTOR) as u32, prp)
    }

    /// Wait for `want` completions, a look first when looks have been
    /// paying, then asleep on the interrupt; `on` is told each one.
    fn reap(&mut self, mut want: usize, mut on: impl FnMut(Completion)) -> Result<(), Status> {
        let started = now_ns();
        let mut looking = self.interrupt.is_none() || self.look.try_look();
        let mut turns = 1u32;
        while want > 0 {
            while let Some(c) = self.io.peek() {
                self.io.pop();
                on(c);
                want -= 1;
                if want == 0 {
                    break;
                }
            }
            if want == 0 {
                break;
            }
            match self.interrupt {
                Some(irq) if !looking => claim::interrupt_wait(irq)?,
                Some(_) => {
                    if turns.is_multiple_of(4096) && now_ns() - started >= DEVICE_LOOK_NS {
                        self.look.done(false);
                        looking = false;
                    }
                    turns += 1;
                    core::hint::spin_loop();
                }
                None => core::hint::spin_loop(),
            }
        }
        if looking && self.interrupt.is_some() {
            self.look.done(true);
        }
        self.io.release(&self.regs);
        Ok(())
    }
}

/// One admin command, polled until it is answered.
fn admin_call(regs: &Regs, admin: &mut Pair, cmd: &Command) -> Result<(), String> {
    admin.push(cmd);
    admin.ring(regs);
    let start = now_ns();
    let c = loop {
        if let Some(c) = admin.peek() {
            break c;
        }
        if now_ns() - start > READY_NS {
            return Err(format!(
                "admin command {:#x} was never answered",
                cmd.opcode
            ));
        }
        core::hint::spin_loop();
    };
    admin.pop();
    admin.release(regs);
    if c.status != 0 {
        return Err(format!(
            "admin command {:#x} failed: status {:#x}",
            cmd.opcode, c.status
        ));
    }
    Ok(())
}

impl Backend for Nvme {
    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn has_flush(&self) -> bool {
        self.has_flush
    }

    fn run(&mut self, ios: &[Io], done: &mut [bool]) -> Result<(), Status> {
        done.iter_mut().for_each(|d| *d = true);
        // Every piece of every request, `MAX_CMDS` at a time: the request
        // each command is a piece of, by its command id.
        let mut of = [0usize; MAX_CMDS];
        let mut n = 0;
        let max = self.max_xfer;
        let send = |nvme: &mut Nvme, of: &[usize], done: &mut [bool]| -> Result<(), Status> {
            nvme.io.ring(&nvme.regs);
            nvme.reap(of.len(), |c| {
                if c.status != 0 {
                    if let Some(&i) = of.get(c.cid as usize) {
                        done[i] = false;
                    }
                }
            })
        };
        for (i, io) in ios.iter().enumerate() {
            for piece in pieces(io, max) {
                if n == MAX_CMDS {
                    send(self, &of[..n], done)?;
                    n = 0;
                }
                let mut cmd = self.command(io, piece, n);
                cmd.cid = n as u16;
                self.io.push(&cmd);
                of[n] = i;
                n += 1;
            }
        }
        if n > 0 {
            send(self, &of[..n], done)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blk::{check, Msg, OP_FLUSH, OP_READ, OP_WRITE};

    const P: u64 = PAGE as u64;

    #[test]
    fn a_command_is_the_sixty_four_bytes_the_controller_reads() {
        let c = Command::rw(true, 0x1_0000_0002, 8, (0x5000, 0x6000));
        let d = Command { cid: 7, ..c }.encode();
        assert_eq!(d[0], NVM_WRITE as u32 | 7 << 16);
        assert_eq!(d[1], NSID);
        assert_eq!((d[6], d[7], d[8], d[9]), (0x5000, 0, 0x6000, 0));
        assert_eq!((d[10], d[11]), (2, 1), "the LBA, low then high");
        assert_eq!(d[12], 7, "the count is zero-based");
        assert_eq!(Command::rw(false, 0, 1, (0, 0)).opcode, NVM_READ);
    }

    #[test]
    fn a_completion_says_which_command_how_it_went_and_whether_it_is_new() {
        let c = Completion::decode([0, 0, 0x0001_0005, 0x0003_0009]);
        assert_eq!((c.cid, c.sq_head, c.phase, c.status), (9, 5, true, 1));
        let c = Completion::decode([0, 0, 0, 0x0000_0004]);
        assert_eq!((c.cid, c.phase, c.status), (4, false, 0));
    }

    #[test]
    fn a_transfer_in_one_page_needs_one_pointer_and_in_two_the_second_page() {
        let mut list = [0u64; 512];
        assert_eq!(
            prps(0x10_0000, 4096, &mut list, 0x9000),
            Some((0x10_0000, 0))
        );
        assert_eq!(
            prps(0x10_0200, 512, &mut list, 0x9000),
            Some((0x10_0200, 0))
        );
        // Off the start of its page: its end is in the next one.
        assert_eq!(
            prps(0x10_0200, 4096, &mut list, 0x9000),
            Some((0x10_0200, 0x10_1000))
        );
        assert_eq!(
            prps(0x10_0000, 8192, &mut list, 0x9000),
            Some((0x10_0000, 0x10_1000))
        );
        assert_eq!(list[0], 0, "no list for two pages");
    }

    #[test]
    fn a_longer_transfer_has_every_page_after_the_first_in_a_list() {
        let mut list = [0u64; 512];
        assert_eq!(
            prps(0x10_0000, 64 * 1024, &mut list, 0x9000),
            Some((0x10_0000, 0x9000))
        );
        assert_eq!(
            &list[..15],
            &(1..16).map(|i| 0x10_0000 + i * P).collect::<Vec<_>>()[..]
        );
        assert_eq!(list[15], 0);
        // A megabyte, which is the window: one list page holds it.
        assert!(prps(0x10_0000, DATA_MAX, &mut list, 0x9000).is_some());
        let mut short = [0u64; 3];
        assert_eq!(prps(0x10_0000, 5 * PAGE, &mut short, 0x9000), None);
    }

    fn io(op: u32, sector: u64, sectors: u32, off: u32) -> Io {
        let m = Msg {
            op,
            sectors,
            sector,
            buf_off: off,
            ..Msg::default()
        };
        check(&m, 1 << 20).unwrap()
    }

    #[test]
    fn a_request_longer_than_one_command_is_cut_in_pieces() {
        let big = io(OP_WRITE, 100, 256, 4096); // 128 KiB
        let p: Vec<_> = pieces(&big, 64 * 1024).collect();
        assert_eq!(p, [(100, 65536, 4096), (228, 65536, 4096 + 65536)]);
        let odd = io(OP_READ, 0, 130, 0); // 65 KiB
        let p: Vec<_> = pieces(&odd, 64 * 1024).collect();
        assert_eq!(p, [(0, 65536, 0), (128, 1024, 65536)]);
        let small = io(OP_READ, 9, 8, 0);
        assert_eq!(pieces(&small, 64 * 1024).count(), 1);
        let flush = io(OP_FLUSH, 0, 0, 0);
        assert_eq!(
            pieces(&flush, 64 * 1024).count(),
            1,
            "a flush is one command"
        );
    }

    #[test]
    fn the_dma_area_has_room_for_every_command_s_list() {
        assert_eq!(DMA_DATA % PAGE, 0);
        assert_eq!(DMA_SIZE, DMA_DATA + DATA_MAX);
    }
}
