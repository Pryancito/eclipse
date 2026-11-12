//! A virtio disk, served to the kernel.
//!
//! The kernel sends 32-byte messages down a FIFO -- an operation, a range
//! of sectors and where in the data window the bytes are -- and the same
//! message comes back with a status (`loader/src/linux/block_serve.rs` is
//! the other end). The window is a slice of the driver's DMA memory that
//! the device reads and writes directly: no copy on either side.

use core::ptr;

use crate::claim;
use crate::fifo::{Fifo, Message};
use crate::hybrid::Hybrid;
use crate::queue::{Desc, Queue, DESC_F_NEXT, DESC_F_WRITE};
use crate::virtio::Device;
use crate::zx::{Handle, Status};
use crate::{now_ns, PAGE};

pub const SECTOR: usize = 512;

/// The operations of a [`Msg`].
pub const OP_READ: u32 = 1;
pub const OP_WRITE: u32 = 2;
/// What was written, onto the disk: no sectors, no data.
pub const OP_FLUSH: u32 = 4;

/// Messages the FIFO holds each way: one per request the kernel can have
/// outstanding.
pub const FIFO_DEPTH: usize = 16;
/// Requests on the device at once.
pub const MAX_INFLIGHT: usize = 16;
/// Descriptors in the queue: three per request in flight, and room.
pub const QUEUE_SIZE: u16 = 64;

/// Feature bit 9: the device has a write cache and takes a flush request.
/// A driver that does not accept it gets a device that syncs every write
/// (QEMU does exactly that), which makes each one cost a flush.
pub const F_FLUSH: u32 = 1 << 9;

// virtio-blk requests (virtio 1.1 §5.2.6).
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const S_OK: u8 = 0;

/// The DMA area, one contiguous VMO: the queue in the first two pages, a
/// page of request headers and statuses (one [`REQ_SLOT`] per request in
/// flight), then the data window.
pub const DMA_QUEUE: usize = 0;
pub const DMA_REQ: usize = 2 * PAGE;
pub const DMA_DATA: usize = 3 * PAGE;
pub const DATA_MAX: usize = 64 * 1024;
pub const DMA_SIZE: usize = DMA_DATA + DATA_MAX;
const REQ_SLOT: usize = 32;
/// A request's header: type, a reserved word and the sector. Its status
/// byte follows.
const REQ_HEADER: usize = 16;
const _: () = assert!(MAX_INFLIGHT * REQ_SLOT <= PAGE);
const _: () = assert!(3 * MAX_INFLIGHT <= QUEUE_SIZE as usize);

/// How long a wait looks before it sleeps (see [`Hybrid`]): on the used
/// ring before the interrupt, on the FIFO before sleeping on it.
const DEVICE_LOOK_NS: u64 = 200_000;
const FIFO_LOOK_NS: u64 = 300_000;

/// One FIFO element.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Msg {
    pub op: u32,
    pub reqid: u32,
    pub sectors: u32,
    /// Bytes into the data window.
    pub buf_off: u32,
    pub sector: u64,
    /// 0 or a negative `zx_status_t`, on the way back.
    pub status: i64,
}
const _: () = assert!(core::mem::size_of::<Msg>() == 32);
// SAFETY: `repr(C)`, 32 bytes of integers with no padding.
unsafe impl Message for Msg {}

/// One request for the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Io {
    kind: u32,
    sector: u64,
    /// Bytes of data, and where they are in the data window.
    len: usize,
    off: usize,
}

impl Io {
    fn is_flush(&self) -> bool {
        self.kind == T_FLUSH
    }
}

/// A request, checked: what to ask the device, or the status to send back
/// without going near it. The kernel's word, all of it: nothing reaches the
/// device that does not fit the window and the disk of `capacity` sectors.
pub fn check(m: &Msg, capacity: u64) -> Result<Io, Status> {
    if m.op == OP_FLUSH {
        if m.sectors != 0 {
            return Err(Status::INVALID_ARGS);
        }
        return Ok(Io {
            kind: T_FLUSH,
            sector: 0,
            len: 0,
            off: 0,
        });
    }
    if m.op != OP_READ && m.op != OP_WRITE {
        return Err(Status::INVALID_ARGS);
    }
    let len = m.sectors as usize * SECTOR;
    let off = m.buf_off as usize;
    if m.sectors == 0 || !off.is_multiple_of(SECTOR) || len > DATA_MAX || off > DATA_MAX - len {
        return Err(Status::OUT_OF_RANGE);
    }
    if m.sector > capacity || m.sectors as u64 > capacity - m.sector {
        return Err(Status::OUT_OF_RANGE);
    }
    Ok(Io {
        kind: if m.op == OP_READ { T_IN } else { T_OUT },
        sector: m.sector,
        len,
        off,
    })
}

/// Answer the requests in `msgs`: each one's status is set, and those that
/// are for the device go to `run` in batches -- everything one read of the
/// FIFO brought is on the device at once (up to [`MAX_INFLIGHT`], in
/// order), so a kernel with several requests outstanding pays one notify,
/// one interrupt and one wake for all of them.
///
/// A flush goes to the device alone, after everything before it has come
/// back: it is those writes it is asked to put on the disk. A device
/// without a write cache has nothing to flush, since every write it
/// answered is already there.
///
/// `run` is handed a batch and says, for each request, whether the device
/// did it.
pub fn answer(
    msgs: &mut [Msg],
    capacity: u64,
    has_flush: bool,
    mut run: impl FnMut(&[Io], &mut [bool]),
) {
    let mut ios = [Io {
        kind: 0,
        sector: 0,
        len: 0,
        off: 0,
    }; MAX_INFLIGHT];
    let mut at = [0usize; MAX_INFLIGHT]; // which message each of `ios` answers
    let mut n = 0;
    let mut flush = |ios: &[Io], at: &[usize], msgs: &mut [Msg]| {
        let mut done = [false; MAX_INFLIGHT];
        run(ios, &mut done[..ios.len()]);
        for (k, &i) in at.iter().enumerate() {
            msgs[i].status = if done[k] { 0 } else { Status::IO.0 as i64 };
        }
    };
    for i in 0..msgs.len() {
        let alone = n == 1 && ios[0].is_flush();
        if n > 0 && (msgs[i].op == OP_FLUSH || alone || n == MAX_INFLIGHT) {
            flush(&ios[..n], &at[..n], msgs);
            n = 0;
        }
        match check(&msgs[i], capacity) {
            Err(status) => msgs[i].status = status.0 as i64,
            Ok(io) if io.is_flush() && !has_flush => msgs[i].status = 0,
            Ok(io) => {
                msgs[i].status = 0;
                ios[n] = io;
                at[n] = i;
                n += 1;
            }
        }
    }
    if n > 0 {
        flush(&ios[..n], &at[..n], msgs);
    }
}

/// The disk: the device, its queue and its DMA memory.
pub struct Disk {
    device: Device,
    queue: Queue,
    /// The DMA area ([`DMA_SIZE`] bytes), as this process and as the device
    /// see it.
    dma: *mut u8,
    daddr: u64,
    /// The interrupt to sleep on, `None` to poll the used ring.
    interrupt: Option<Handle>,
    look: Hybrid,
    pub capacity: u64,
    pub has_flush: bool,
}

impl Disk {
    /// The disk behind `device`, already brought up with its one queue in
    /// the first two pages of the DMA area.
    ///
    /// # Safety
    /// `dma` is [`DMA_SIZE`] bytes of DMA memory the device sees at `daddr`,
    /// mapped for as long as the `Disk` lives and touched by nobody else
    /// but the device and, in the data window, whoever sends the requests.
    pub unsafe fn new(
        device: Device,
        dma: *mut u8,
        daddr: u64,
        interrupt: Option<Handle>,
        capacity: u64,
        has_flush: bool,
    ) -> Disk {
        Disk {
            device,
            queue: Queue::new(dma.add(DMA_QUEUE), QUEUE_SIZE),
            dma,
            daddr,
            interrupt,
            look: Hybrid::new(true),
            capacity,
            has_flush,
        }
    }

    /// Lay `ios` out for the device: a header, the data and a status byte
    /// each, three descriptors chained, all in the available ring. Nothing
    /// is the device's until the ring is published.
    fn stage(&mut self, ios: &[Io]) {
        assert!(ios.len() <= MAX_INFLIGHT);
        for (i, io) in ios.iter().enumerate() {
            let slot = DMA_REQ + i * REQ_SLOT;
            let header = self.daddr + slot as u64;
            // SAFETY: the request's slot, inside the page of headers
            // (`new`), aligned for the words written.
            unsafe {
                let req = self.dma.add(slot);
                ptr::write_volatile(req as *mut u32, io.kind);
                ptr::write_volatile(req.add(4) as *mut u32, 0);
                ptr::write_volatile(req.add(8) as *mut u64, io.sector);
                ptr::write_volatile(req.add(REQ_HEADER), 0xff);
            }
            let d = (3 * i) as u16;
            // A flush carries no data: its header chains straight to its
            // status.
            self.queue.set_desc(
                d,
                Desc {
                    addr: header,
                    len: REQ_HEADER as u32,
                    flags: DESC_F_NEXT,
                    next: if io.is_flush() { d + 2 } else { d + 1 },
                },
            );
            self.queue.set_desc(
                d + 1,
                Desc {
                    addr: self.daddr + (DMA_DATA + io.off) as u64,
                    len: io.len as u32,
                    flags: DESC_F_NEXT | if io.kind == T_IN { DESC_F_WRITE } else { 0 },
                    next: d + 2,
                },
            );
            self.queue.set_desc(
                d + 2,
                Desc {
                    addr: header + REQ_HEADER as u64,
                    len: 1,
                    flags: DESC_F_WRITE,
                    next: 0,
                },
            );
            self.queue.offer(i as u16, d);
        }
    }

    /// Whether the device did request `i` of the batch it has answered.
    fn status(&self, i: usize) -> bool {
        // SAFETY: the status byte of the request's slot.
        unsafe { ptr::read_volatile(self.dma.add(DMA_REQ + i * REQ_SLOT + REQ_HEADER)) == S_OK }
    }

    /// Put `ios` on the device, all at once, and wait until it has answered
    /// every one: a look at the used ring first when looks have been
    /// paying, then asleep on the interrupt.
    pub fn run(&mut self, ios: &[Io], done: &mut [bool]) -> Result<(), Status> {
        self.stage(ios);
        let n = ios.len() as u16;
        self.queue.publish(n);
        let want = self.queue.last_used().wrapping_add(n);
        let notified = now_ns();
        self.device.notify(0);
        match self.interrupt {
            Some(interrupt) => {
                // A flush takes as long as the disk under the device does:
                // no look for it, and nothing learnt from it about how fast
                // the device answers.
                if !ios[0].is_flush() && self.look.try_look() {
                    // The clock is a system call where there is no vDSO:
                    // look at it now and then, not on every turn.
                    let mut turns = 1u32;
                    while self.queue.used_idx() != want {
                        if turns.is_multiple_of(4096) && now_ns() - notified >= DEVICE_LOOK_NS {
                            break;
                        }
                        turns += 1;
                        core::hint::spin_loop();
                    }
                    self.look.done(self.queue.used_idx() == want);
                }
                // A wake is not a completion (a stale one, a config change,
                // one interrupt for part of the batch): look at the ring
                // after each.
                while self.queue.used_idx() != want {
                    claim::interrupt_wait(interrupt)?;
                    self.device.ack();
                }
            }
            None => {
                while self.queue.used_idx() != want {
                    core::hint::spin_loop();
                }
                // Nobody waits on the interrupt, but the line stays up
                // until acked.
                self.device.ack();
            }
        }
        self.queue.take_upto(want);
        for (i, done) in done.iter_mut().enumerate() {
            *done = self.status(i);
        }
        Ok(())
    }
}

/// Wait for requests on `fifo` and read them: with `look`, a look that
/// [`Hybrid::try_look`] has allowed, reading for a while first; then
/// asleep.
fn wait_requests(
    fifo: &Fifo<Msg>,
    look: Option<&mut Hybrid>,
    msgs: &mut [Msg],
) -> Result<usize, Status> {
    if let Some(look) = look {
        let started = now_ns();
        let mut turns = 1u32;
        loop {
            match fifo.read(msgs) {
                Ok(got) => {
                    look.done(true);
                    return Ok(got);
                }
                Err(Status::SHOULD_WAIT) => {}
                Err(e) => return Err(e),
            }
            // The clock is a system call where there is no vDSO: look at
            // it now and then.
            if turns.is_multiple_of(16) && now_ns() - started >= FIFO_LOOK_NS {
                look.done(false);
                break;
            }
            turns += 1;
        }
    }
    fifo.reply_wait(msgs, 0)
}

/// One turn: the first `replies` messages of `msgs` back to the kernel and
/// its next requests into `msgs`.
///
/// Asleep from the start with one reply to give (or none), that is one
/// call: against a write, a read and a wait, a 4 KiB write to the raw disk
/// takes 15 % less at one CPU, and one through vfat 27 % less. A look reads
/// the FIFO from here, so the replies go first, in a call of their own. So
/// do several replies: they wake several clients, and the one call then
/// measured 5 % slower with sixteen threads reading at once.
fn turn(
    fifo: &Fifo<Msg>,
    look: &mut Hybrid,
    msgs: &mut [Msg],
    replies: usize,
) -> Result<usize, Status> {
    let looking = look.try_look();
    if !looking && replies <= 1 {
        return fifo.reply_wait(msgs, replies);
    }
    // The kernel never has more than FIFO_DEPTH outstanding, so the replies
    // always fit.
    if replies > 0 && fifo.write(&msgs[..replies])? != replies {
        return Err(Status::SHOULD_WAIT);
    }
    wait_requests(fifo, looking.then_some(look), msgs)
}

/// Serve `disk` on `fifo` until the kernel's end goes away (`PEER_CLOSED`)
/// or the device stops answering. The status that ended it.
pub fn serve(disk: &mut Disk, fifo: &Fifo<Msg>) -> Status {
    let mut msgs = [Msg::default(); FIFO_DEPTH];
    let mut look = Hybrid::new(true);
    // The replies to the requests of the turn before.
    let mut got = 0;
    loop {
        got = match turn(fifo, &mut look, &mut msgs, got) {
            Ok(got) => got,
            Err(status) => return status,
        };
        let (capacity, has_flush) = (disk.capacity, disk.has_flush);
        let mut failed = None;
        answer(&mut msgs[..got], capacity, has_flush, |ios, done| {
            if failed.is_none() {
                if let Err(status) = disk.run(ios, done) {
                    failed = Some(status);
                }
            }
        });
        if let Some(status) = failed {
            return status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::tests::Pages;
    use crate::virtio::Transport;

    const CAPACITY: u64 = 2048;

    fn msg(op: u32, sector: u64, sectors: u32, buf_off: u32) -> Msg {
        Msg {
            op,
            reqid: 1,
            sectors,
            buf_off,
            sector,
            status: 77,
        }
    }

    /// `answer` over `msgs` with a device that does everything it is asked
    /// except the requests at sector `bad`: the batches it was handed.
    fn batches(msgs: &mut [Msg], has_flush: bool, bad: Option<u64>) -> Vec<Vec<Io>> {
        let mut seen = Vec::new();
        answer(msgs, CAPACITY, has_flush, |ios, done| {
            seen.push(ios.to_vec());
            for (io, done) in ios.iter().zip(done.iter_mut()) {
                *done = Some(io.sector) != bad || io.is_flush();
            }
        });
        seen
    }

    #[test]
    fn a_message_is_the_thirty_two_bytes_the_kernel_writes() {
        // loader/src/linux/block_serve.rs `encode`: op, id, sectors, offset
        // as u32s, then the sector and the status as 64-bit words.
        let m = Msg {
            op: OP_WRITE,
            reqid: 0x31,
            sectors: 8,
            buf_off: 3 * 4096,
            sector: 9,
            status: -40,
        };
        // SAFETY: `Msg` is 32 bytes without padding.
        let bytes: [u8; 32] = unsafe { core::mem::transmute(m) };
        assert_eq!(&bytes[0..4], &OP_WRITE.to_ne_bytes());
        assert_eq!(&bytes[4..8], &0x31u32.to_ne_bytes());
        assert_eq!(&bytes[8..12], &8u32.to_ne_bytes());
        assert_eq!(&bytes[12..16], &(3u32 * 4096).to_ne_bytes());
        assert_eq!(&bytes[16..24], &9u64.to_ne_bytes());
        assert_eq!(&bytes[24..32], &(-40i64).to_ne_bytes());
    }

    #[test]
    fn a_read_and_a_write_in_range_go_to_the_device_as_asked() {
        let io = check(&msg(OP_READ, 100, 8, 4096), CAPACITY).unwrap();
        assert_eq!(
            (io.kind, io.sector, io.len, io.off),
            (T_IN, 100, 4096, 4096)
        );
        let io = check(&msg(OP_WRITE, 0, 1, 0), CAPACITY).unwrap();
        assert_eq!((io.kind, io.len), (T_OUT, 512));
        // The whole window, and the last sectors of the disk.
        assert!(check(&msg(OP_READ, 0, 128, 0), CAPACITY).is_ok());
        assert!(check(&msg(OP_READ, CAPACITY - 8, 8, 0), CAPACITY).is_ok());
    }

    #[test]
    fn what_does_not_fit_the_window_or_the_disk_never_reaches_the_device() {
        let out = Err(Status::OUT_OF_RANGE);
        assert_eq!(check(&msg(OP_READ, 0, 0, 0), CAPACITY), out);
        assert_eq!(
            check(&msg(OP_READ, 0, 129, 0), CAPACITY),
            out,
            "more than the window"
        );
        assert_eq!(
            check(&msg(OP_READ, 0, 8, 61 * 1024), CAPACITY),
            out,
            "off its end"
        );
        assert_eq!(
            check(&msg(OP_READ, 0, 1, 100), CAPACITY),
            out,
            "off the sector grid"
        );
        assert_eq!(check(&msg(OP_WRITE, CAPACITY - 7, 8, 0), CAPACITY), out);
        assert_eq!(check(&msg(OP_WRITE, CAPACITY + 1, 1, 0), CAPACITY), out);
        assert_eq!(check(&msg(OP_READ, u64::MAX, 8, 0), CAPACITY), out);
        assert_eq!(
            check(&msg(OP_READ, 0, u32::MAX, 0), CAPACITY),
            out,
            "a length that would wrap"
        );
        assert_eq!(check(&msg(OP_READ, 0, 1, u32::MAX - 511), CAPACITY), out);
    }

    #[test]
    fn an_operation_nobody_defined_and_a_flush_with_sectors_are_refused() {
        let bad = Err(Status::INVALID_ARGS);
        assert_eq!(check(&msg(0, 0, 1, 0), CAPACITY), bad);
        assert_eq!(check(&msg(3, 0, 1, 0), CAPACITY), bad);
        assert_eq!(check(&msg(OP_FLUSH, 0, 1, 0), CAPACITY), bad);
        assert!(check(&msg(OP_FLUSH, 0, 0, 0), CAPACITY).unwrap().is_flush());
    }

    #[test]
    fn everything_one_read_brought_is_on_the_device_at_once() {
        let mut msgs: Vec<Msg> = (0..5)
            .map(|i| msg(OP_READ, i * 8, 8, i as u32 * 4096))
            .collect();
        let seen = batches(&mut msgs, true, None);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].len(), 5);
        assert_eq!(seen[0][3].sector, 24);
        assert!(msgs.iter().all(|m| m.status == 0));
    }

    #[test]
    fn a_request_the_device_fails_answers_io_and_its_neighbours_do_not() {
        let mut msgs = [
            msg(OP_READ, 0, 8, 0),
            msg(OP_READ, 8, 8, 4096),
            msg(OP_READ, 16, 8, 8192),
        ];
        batches(&mut msgs, true, Some(8));
        assert_eq!(msgs[0].status, 0);
        assert_eq!(msgs[1].status, Status::IO.0 as i64);
        assert_eq!(msgs[2].status, 0);
    }

    #[test]
    fn a_refused_request_is_answered_without_the_device_and_the_rest_go_on() {
        let mut msgs = [
            msg(OP_READ, 0, 8, 0),
            msg(OP_READ, CAPACITY, 8, 4096),
            msg(9, 0, 8, 0),
            msg(OP_WRITE, 16, 8, 8192),
        ];
        let seen = batches(&mut msgs, true, None);
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].iter().map(|io| io.sector).collect::<Vec<_>>(),
            [0, 16]
        );
        assert_eq!(msgs[0].status, 0);
        assert_eq!(msgs[1].status, Status::OUT_OF_RANGE.0 as i64);
        assert_eq!(msgs[2].status, Status::INVALID_ARGS.0 as i64);
        assert_eq!(msgs[3].status, 0);
    }

    #[test]
    fn a_flush_goes_alone_after_the_writes_before_it_have_come_back() {
        let mut msgs = [
            msg(OP_WRITE, 0, 8, 0),
            msg(OP_WRITE, 8, 8, 4096),
            msg(OP_FLUSH, 0, 0, 0),
            msg(OP_WRITE, 16, 8, 0),
            msg(OP_READ, 24, 8, 4096),
        ];
        let seen = batches(&mut msgs, true, None);
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].len(), 2);
        assert!(seen[1].len() == 1 && seen[1][0].is_flush());
        assert_eq!(seen[2].len(), 2);
        assert!(msgs.iter().all(|m| m.status == 0));
    }

    #[test]
    fn a_device_without_a_write_cache_is_not_asked_to_flush() {
        let mut msgs = [
            msg(OP_WRITE, 0, 8, 0),
            msg(OP_FLUSH, 0, 0, 0),
            msg(OP_WRITE, 8, 8, 0),
        ];
        let seen = batches(&mut msgs, false, None);
        // The flush still waits for the write before it: that write is on
        // the disk when the flush is answered.
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().flatten().all(|io| !io.is_flush()));
        assert_eq!(msgs[1].status, 0);
    }

    #[test]
    fn no_more_goes_to_the_device_at_once_than_it_has_room_for() {
        // The FIFO holds no more than the device takes, but the cut must
        // not depend on that.
        let mut msgs: Vec<Msg> = (0..20).map(|i| msg(OP_READ, i * 8, 8, 0)).collect();
        let seen = batches(&mut msgs, true, None);
        assert_eq!(
            seen.iter().map(Vec::len).collect::<Vec<_>>(),
            [MAX_INFLIGHT, 4]
        );
    }

    #[test]
    fn an_empty_turn_asks_the_device_nothing() {
        assert!(batches(&mut [], true, None).is_empty());
    }

    #[test]
    fn a_batch_is_laid_out_as_three_chained_buffers_a_request() {
        let mut dma = Pages::new(DMA_SIZE / PAGE);
        let mut regs = Pages::new(1);
        let daddr = 0x4_0000_0000u64;
        let device = unsafe { Device::new(regs.ptr(), Transport::Mmio) };
        let mut disk = unsafe { Disk::new(device, dma.ptr(), daddr, None, CAPACITY, true) };
        let ios = [
            check(&msg(OP_READ, 100, 8, 4096), CAPACITY).unwrap(),
            check(&msg(OP_WRITE, 7, 1, 0), CAPACITY).unwrap(),
            check(&msg(OP_FLUSH, 0, 0, 0), CAPACITY).unwrap(),
        ];
        disk.stage(&ios);
        // The read: header, 4 KiB the device writes into, a status byte it
        // writes.
        let header = daddr + DMA_REQ as u64;
        assert_eq!(
            disk.queue.desc(0),
            Desc {
                addr: header,
                len: 16,
                flags: DESC_F_NEXT,
                next: 1
            }
        );
        assert_eq!(
            disk.queue.desc(1),
            Desc {
                addr: daddr + (DMA_DATA + 4096) as u64,
                len: 4096,
                flags: DESC_F_NEXT | DESC_F_WRITE,
                next: 2
            }
        );
        assert_eq!(
            disk.queue.desc(2),
            Desc {
                addr: header + 16,
                len: 1,
                flags: DESC_F_WRITE,
                next: 0
            }
        );
        // The write: the device only reads its data.
        assert_eq!(disk.queue.desc(4).flags, DESC_F_NEXT);
        assert_eq!(disk.queue.desc(4).len, 512);
        // The flush: its header chains straight to its status.
        assert_eq!(disk.queue.desc(6).next, 8);
        assert_eq!(disk.queue.desc(8).addr, header + 2 * 32 + 16);
        // The headers, as virtio-blk reads them, each with a status the
        // device has not written yet.
        let bytes = dma.bytes();
        let req = |i: usize| &bytes[DMA_REQ + i * 32..DMA_REQ + i * 32 + 17];
        assert_eq!(&req(0)[0..4], &0u32.to_le_bytes());
        assert_eq!(&req(0)[8..16], &100u64.to_le_bytes());
        assert_eq!(&req(1)[0..4], &1u32.to_le_bytes());
        assert_eq!(&req(1)[8..16], &7u64.to_le_bytes());
        assert_eq!(&req(2)[0..4], &4u32.to_le_bytes());
        assert!((0..3).all(|i| req(i)[16] == 0xff));
        assert!(!disk.status(0));
        // The three chains are in the available ring, not yet published.
        let avail = crate::queue::avail_offset(QUEUE_SIZE);
        let ring =
            |k: usize| u16::from_le_bytes([bytes[avail + 4 + 2 * k], bytes[avail + 5 + 2 * k]]);
        assert_eq!([ring(0), ring(1), ring(2)], [0, 3, 6]);
        assert_eq!(disk.queue.avail_idx(), 0);
    }
}
