//! Simulating a fast disk with a physics core.
//!
//! One core runs [`mocking`] forever: it drains the submit queue, copies each
//! request through a RAM-backed image, and charges the copy ten times over on
//! top of itself so the simulated disk runs about eleven times slower than
//! memory. Clients on the other cores block in `wait_for_interrupt` until
//! their completion word is written and the disk's IPI wakes them.
//! Synchronisation is that one acquire/release pair plus the IPI; nothing else
//! is shared.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use kernel_hal::{
    cpu::cpu_id,
    interrupt::{send_ipi, wait_for_interrupt},
    timer::timer_now,
    IpiReason, LazyInit, MpscQueue,
};

type SubmitQueue = MpscQueue<'static, Entry>;

static SQ: LazyInit<Arc<SubmitQueue>> = LazyInit::new();

/// The completion word that says a request moved `moved` bytes.
///
/// The byte count **plus one**, because the client spins on `finish == 0` and
/// zero is therefore already spoken for as "not done yet" -- while a request
/// the image cannot serve moves zero bytes and still has to wake its CPU.
const fn encode_done(moved: usize) -> usize {
    moved + 1
}

/// The byte count in a completion word, or `None` while the request is still
/// outstanding.
const fn decode_done(word: usize) -> Option<usize> {
    match word {
        0 => None,
        w => Some(w - 1),
    }
}

/// Submiting(client) side of the mock disk
pub struct MockBlock {
    sq: Arc<SubmitQueue>,
}

impl MockBlock {
    /// Wait until the mock disk is inited
    pub fn new() -> Self {
        while !MOCK_DISK_READY.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        Self { sq: SQ.clone() }
    }

    fn submit_entry(&self, start: EntryType, op: OpCode, buf: &[u8], finish: *const AtomicUsize) {
        let idx = loop {
            if let Some(idx) = self.sq.alloc_entry() {
                break idx;
            }
            core::hint::spin_loop();
        };
        let entry = self.sq.entry_at(idx);
        entry.start = start;
        entry.op = op;
        entry.buf_ptr = buf.as_ptr() as _;
        entry.buf_size = buf.len();
        entry.cpuid = cpu_id() as _;
        entry.finish = finish;
        // Before `commit_entry`, not after. Publishing hands the slot to the
        // consumer, which may drain it and let another producer's
        // `alloc_entry` wrap onto the same index -- `entry_at` indexes
        // `idx % len` -- before this line runs. The trace then prints whatever
        // landed there: another CPU's request, or half of one.
        trace!("entry submit : {:#x?} @ {}", entry, idx);
        self.sq.commit_entry(idx);
    }

    /// Submit one request and block until the disk answers, giving the number
    /// of bytes it moved.
    fn transfer(&self, start: EntryType, op: OpCode, buf: &[u8]) -> usize {
        // The box outlives the wait below, so the pointer the disk stores
        // through stays good for as long as the disk can reach it.
        let finish = Box::new(AtomicUsize::new(0));
        self.submit_entry(start, op, buf, &*finish);
        loop {
            // Read through the box. Only the disk side needs the raw pointer,
            // and `AtomicUsize::store` takes `&self`, so nothing here has to
            // make a `&mut` to memory another core is writing.
            if let Some(moved) = decode_done(finish.load(Ordering::Acquire)) {
                return moved;
            }
            wait_for_interrupt();
        }
    }
}

impl Default for MockBlock {
    fn default() -> Self {
        Self::new()
    }
}

use rcore_fs::dev::{Device, Result};

impl Device for MockBlock {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        Ok(self.transfer(EntryType::Offset(offset), OpCode::Read, buf))
    }
    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        Ok(self.transfer(EntryType::Offset(offset), OpCode::Write, buf))
    }
    fn sync(&self) -> Result<()> {
        Ok(())
    }
}

/// Requests whose bytes are already copied and whose completion the disk still
/// owes, earliest due first.
///
/// The key is `(due, seq)` and not `due` alone. `BTreeMap::insert` **replaces**,
/// and the entry it would drop here is one whose completion word is never
/// written and whose IPI is never sent: the CPU that submitted it spins in
/// `wait_for_interrupt` for good, and with `MOCK=1` this disk is the rootfs
/// device, so that is a boot that never finishes. Two requests share a `due`
/// easily -- `handle_submits` drains a whole batch in one pass, a request is a
/// 512-byte copy, and `timer_now` advances once every 16 ns on QEMU's aarch64
/// `virt` (CNTVCT_EL0 at 62.5 MHz), so both readings land on the same tick for
/// each of them and [`finish_time`] collapses to that tick. `seq` also keeps
/// requests that come due together in the order they were submitted.
#[derive(Default)]
struct Pending {
    queue: BTreeMap<(u128, u64), (Entry, usize)>,
    next_seq: u64,
}

impl Pending {
    /// Owe one more completion, due at `due`, for a request that moved `moved`
    /// bytes.
    fn push(&mut self, due: u128, entry: Entry, moved: usize) {
        self.queue.insert((due, self.next_seq), (entry, moved));
        self.next_seq = self.next_seq.wrapping_add(1);
    }

    /// Take the earliest request due at or before `now`.
    fn pop_due(&mut self, now: u128) -> Option<(Entry, usize)> {
        let (&key, _) = self.queue.first_key_value()?;
        if key.0 > now {
            return None;
        }
        self.queue.remove(&key)
    }

    /// How many completions are still owed.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.queue.len()
    }
}

/// When a request submitted at `stime` and copied by `etime` comes due: the
/// copy charged ten times over on top of itself.
///
/// `saturating_sub` and not `-`: `timer_now` is not monotonic on every path --
/// x86_64 only floors the clock when the TSC is not invariant, and aarch64
/// reads CNTVCT_EL0 raw -- and one reading lower than the one before it
/// underflows `u128` here into a deadline `now` never reaches. The request then
/// never completes and its CPU hangs, the same way a dropped entry hangs it.
fn finish_time(stime: u128, etime: u128) -> u128 {
    etime + 10 * etime.saturating_sub(stime)
}

struct Mocking {
    data: MemBuf,
    sq: Arc<SubmitQueue>,
    pending: Pending,
}

impl Mocking {
    pub fn new(data: &'static mut [u8], sq: Arc<SubmitQueue>) -> Self {
        Self {
            data: MemBuf(data),
            sq,
            pending: Pending::default(),
        }
    }

    /// Copy one request through and take on its completion, timing the copy
    /// with `clock`.
    ///
    /// The clock is a parameter because the disk's core is the one thing here
    /// no host test can be: it needs its own CPU, `send_ipi` and a real
    /// `timer_now`. With it passed in, the tests drive *this* function --
    /// deadlines, `Pending` and all -- instead of a copy of it that can drift
    /// away from what the disk runs.
    fn accept(&mut self, entry: &mut Entry, mut clock: impl FnMut() -> u128) {
        let stime = clock();
        let moved = self.data.handle_entry(entry);
        let etime = clock();
        self.pending.push(finish_time(stime, etime), *entry, moved);
    }

    /// Answer every request due at or before `now`: store the byte count where
    /// its client is spinning, then ask `wake` for that CPU's IPI.
    #[allow(unsafe_code)]
    fn deliver(&mut self, now: u128, mut wake: impl FnMut(usize)) {
        while let Some((entry, moved)) = self.pending.pop_due(now) {
            // SAFETY: the client that submitted this entry is blocked in
            // `MockBlock::transfer` until this store lands, so the box behind
            // the pointer is still alive.
            unsafe { &*entry.finish }.store(encode_done(moved), Ordering::Release);
            wake(entry.cpuid);
        }
    }

    fn handle_submits(&mut self) {
        let mut batch = self.sq.consume_entrys();
        for (idx, entry) in batch.iter_mut() {
            trace!("entry received : {:#x?} @ {}", entry, idx);
            self.accept(entry, || timer_now().as_nanos());
        }
    }

    fn handle_finished(&mut self) {
        let now = timer_now().as_nanos();
        self.deliver(now, |cpuid| {
            let reason = IpiReason::MockBlock { block_info: 0 };
            // Not `unwrap`. This is the disk's own core inside a `-> !` loop,
            // so a panic here is the whole disk gone and every CPU waiting on
            // it hung -- and `send_ipi` answers `Err` for a `cpuid` that names
            // no CPU, which is a submitter's field and not this core's to
            // trust. A send that fails costs that client only its wakeup: its
            // completion word is already stored, and `wait_for_interrupt`
            // returns on the timer tick too.
            //
            // No host test reaches this line -- `send_ipi` wants a real CPU --
            // so flipping it to `is_ok` changes nothing any test can see. It is
            // the failed send that is worth a line in the log.
            if send_ipi(cpuid, reason.into()).is_err() {
                warn!("mock disk: no IPI route to cpu {}", cpuid);
            }
        });
    }
}

const BLKSIZE: usize = 512;
const CORE_NUM: usize = 4;
const QUEUE_SIZE: usize = 0x100 * CORE_NUM;
static MOCK_DISK_READY: AtomicBool = AtomicBool::new(false);
const ENTRY: Entry = Entry::new();
static mut QUEUE_BUF: [Entry; QUEUE_SIZE] = [ENTRY; QUEUE_SIZE];

/// Start simulating
#[allow(unsafe_code)]
pub fn mocking(initrd: &'static mut [u8]) -> ! {
    // SAFETY: one core reaches this once, before `MOCK_DISK_READY` lets any
    // client in, and `QUEUE_BUF` is named from nowhere else.
    //
    // Spelled the way the IPI ring spells its own queue in
    // `kernel-hal/src/common/ipi.rs`. A `&mut *(&raw mut QUEUE_BUF)` is
    // `clippy::deref_addrof`, which this crate's `deny(warnings)` turns into an
    // error -- so `cargo clippy --features mock-disk` refused to build the
    // crate at all, which is how long it had been since any job compiled this
    // file.
    SQ.init_by(Arc::new(SubmitQueue::new(unsafe {
        core::slice::from_raw_parts_mut((&raw mut QUEUE_BUF).cast::<Entry>(), QUEUE_SIZE)
    })));
    let mut mock = Mocking::new(initrd, SQ.clone());
    MOCK_DISK_READY.store(true, Ordering::Release);
    loop {
        mock.handle_submits();
        mock.handle_finished();
    }
}

#[repr(usize)]
#[derive(Debug, Copy, Clone)]
#[allow(dead_code)]
enum OpCode {
    Read,
    Write,
    Flush,
}

#[derive(Debug, Copy, Clone)]
#[allow(dead_code)]
enum EntryType {
    Block(usize),
    Offset(usize),
}

#[repr(C)]
#[derive(Copy, Clone)]
struct Entry {
    start: EntryType,
    op: OpCode,
    buf_ptr: usize,
    buf_size: usize,
    cpuid: usize,
    /// Safety:
    ///
    /// This access was portected by atomic operations
    finish: *const AtomicUsize,
}

use core::fmt;

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Entry")
            .field("type", &self.start)
            .field("op", &self.op)
            .field("buf_ptr", &self.buf_ptr)
            .field("buf_len", &self.buf_size)
            .field("cpuid", &self.cpuid)
            .field("finish", &self.finish)
            .finish()
    }
}

impl Entry {
    const fn new() -> Self {
        Self {
            start: EntryType::Block(0),
            op: OpCode::Read,
            buf_ptr: 0,
            buf_size: 0,
            cpuid: 0,
            finish: core::ptr::null(),
        }
    }
}

/// The byte range a `len`-byte request at `start` covers inside a `cap`-byte
/// image, or `None` when it does not fit.
///
/// `checked_add` because `start + len` past the top of `usize` wraps to a small
/// `end` that any `end <= cap` test waves through, and the copy then lands
/// somewhere else entirely. And `None` rather than a panic because this runs on
/// the disk's core: `rcore_fs`'s own byte-wise `Device` impl says a read past
/// the end of a device answers `Ok(0)` rather than failing, "which the callers
/// above rely on". Here it asserted instead, inside a `-> !` loop, so the disk
/// died and every CPU waiting on it hung.
fn request_range(start: usize, len: usize, cap: usize) -> Option<Range<usize>> {
    let end = start.checked_add(len)?;
    (end <= cap).then_some(start..end)
}

struct MemBuf(&'static mut [u8]);

impl MemBuf {
    /// Copy one request through, answering how many bytes moved -- zero for a
    /// request the image cannot serve.
    #[allow(unsafe_code)]
    pub fn handle_entry(&mut self, entry: &mut Entry) -> usize {
        match entry.op {
            OpCode::Read => {
                // SAFETY: the client is blocked in `MockBlock::transfer` until
                // this request completes, so its buffer is still there.
                let buf =
                    unsafe { alloc::slice::from_raw_parts_mut(entry.buf_ptr as _, entry.buf_size) };
                match entry.start {
                    EntryType::Block(block_id) => self.read_block(block_id, buf),
                    EntryType::Offset(offset) => self.read_at(offset, buf),
                }
            }
            OpCode::Write => {
                // SAFETY: as above.
                let buf =
                    unsafe { alloc::slice::from_raw_parts(entry.buf_ptr as _, entry.buf_size) };
                match entry.start {
                    EntryType::Block(block_id) => self.write_block(block_id, buf),
                    EntryType::Offset(offset) => self.write_at(offset, buf),
                }
            }
            // Nothing to flush for an image that is already memory, and
            // nothing reaches here anyway: `Device::sync` answers `Ok(())`
            // without submitting a thing.
            OpCode::Flush => 0,
        }
    }

    /// The range block `block_id` covers, when the request really is one whole
    /// block of the image.
    ///
    /// Passing `len` instead of `BLKSIZE` below would be the same program -- the
    /// guard above has already established they are equal -- so no test can
    /// tell the two apart. `BLKSIZE` is written because it is the length the
    /// range is about.
    fn block_range(&self, block_id: usize, len: usize) -> Option<Range<usize>> {
        if len != BLKSIZE {
            return None;
        }
        request_range(block_id.checked_mul(BLKSIZE)?, BLKSIZE, self.0.len())
    }

    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> usize {
        let Some(range) = self.block_range(block_id, buf.len()) else {
            return 0;
        };
        buf.copy_from_slice(&self.0[range]);
        BLKSIZE
    }

    fn write_block(&mut self, block_id: usize, buf: &[u8]) -> usize {
        let Some(range) = self.block_range(block_id, buf.len()) else {
            return 0;
        };
        self.0[range].copy_from_slice(buf);
        BLKSIZE
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let Some(range) = request_range(offset, buf.len(), self.0.len()) else {
            return 0;
        };
        buf.copy_from_slice(&self.0[range]);
        buf.len()
    }

    fn write_at(&mut self, offset: usize, buf: &[u8]) -> usize {
        let Some(range) = request_range(offset, buf.len(), self.0.len()) else {
            return 0;
        };
        self.0[range].copy_from_slice(buf);
        buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// A client's request, filled in the way [`MockBlock::submit_entry`] fills
    /// one in.
    fn request(op: OpCode, start: EntryType, buf: &[u8], finish: &AtomicUsize) -> Entry {
        let mut e = Entry::new();
        e.op = op;
        e.start = start;
        e.buf_ptr = buf.as_ptr() as usize;
        e.buf_size = buf.len();
        e.finish = finish;
        e
    }

    /// A real [`Mocking`] over a RAM image and its own queue slots: the disk's
    /// core, with the clock and the IPI supplied by the test.
    fn disk(image: Vec<u8>) -> Mocking {
        let slots: &'static mut [Entry] =
            Box::leak(alloc::vec![Entry::new(); 64].into_boxed_slice());
        Mocking::new(
            Box::leak(image.into_boxed_slice()),
            Arc::new(MpscQueue::new(slots)),
        )
    }

    /// One `handle_submits` pass over `entries` with the clock frozen at `tick`,
    /// then one `handle_finished` pass at `now`. Answers the CPUs the disk woke,
    /// in the order it woke them.
    fn run(disk: &mut Mocking, entries: &mut [Entry], tick: u128, now: u128) -> Vec<usize> {
        for entry in entries.iter_mut() {
            disk.accept(entry, || tick);
        }
        let mut woken = Vec::new();
        disk.deliver(now, |cpuid| woken.push(cpuid));
        woken
    }

    #[test]
    fn a_whole_batch_inside_one_clock_tick_wakes_every_client() {
        // What the disk's core actually sees: `consume_entrys` hands back the
        // whole batch at once, each request is a 512-byte copy, and CNTVCT_EL0
        // on QEMU's aarch64 `virt` ticks every 16 ns -- so both clock readings
        // land on the same tick for every request in the batch, and every
        // deadline in it collapses to that tick.
        const N: usize = 8;
        let mut d = disk(alloc::vec![0u8; BLKSIZE * N]);
        let words: Vec<AtomicUsize> = (0..N).map(|_| AtomicUsize::new(0)).collect();
        let bufs: Vec<Vec<u8>> = (0..N).map(|i| alloc::vec![i as u8 + 1; BLKSIZE]).collect();
        let mut entries: Vec<Entry> = (0..N)
            .map(|i| {
                let mut e = request(
                    OpCode::Write,
                    EntryType::Offset(i * BLKSIZE),
                    &bufs[i],
                    &words[i],
                );
                e.cpuid = i;
                e
            })
            .collect();

        let woken = run(&mut d, &mut entries, 1_000, 1_000);
        assert_eq!(
            woken,
            (0..N).collect::<Vec<_>>(),
            "el disco no ha despertado a las ocho CPUs una vez cada una"
        );
        for (i, w) in words.iter().enumerate() {
            assert_eq!(
                decode_done(w.load(Ordering::Acquire)),
                Some(BLKSIZE),
                "la peticion {} se ha quedado sin contestar: su CPU gira para siempre en wait_for_interrupt",
                i
            );
        }
        // Y los bytes estan de verdad en la imagen, cada bloque con su marca.
        for i in 0..N {
            assert_eq!(
                d.data.0[i * BLKSIZE],
                i as u8 + 1,
                "el bloque {} lleva los bytes de otra peticion",
                i
            );
        }
    }

    #[test]
    fn two_requests_that_come_due_in_the_same_tick_both_get_answered() {
        let (w0, w1) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let mut p = Pending::default();
        p.push(
            4242,
            request(OpCode::Read, EntryType::Offset(0), &[], &w0),
            BLKSIZE,
        );
        p.push(
            4242,
            request(OpCode::Read, EntryType::Offset(BLKSIZE), &[], &w1),
            BLKSIZE,
        );
        assert_eq!(
            p.len(),
            2,
            "la segunda peticion del mismo tick ha desaparecido de la cola, y su CPU no despierta nunca"
        );

        let mut answered = 0;
        while p.pop_due(4242).is_some() {
            answered += 1;
        }
        assert_eq!(answered, 2, "el disco solo ha contestado a una");
    }

    #[test]
    fn requests_that_come_due_together_come_out_in_the_order_they_arrived() {
        let w = AtomicUsize::new(0);
        let mut p = Pending::default();
        for moved in [10, 20, 30] {
            p.push(
                7,
                request(OpCode::Read, EntryType::Offset(0), &[], &w),
                moved,
            );
        }
        let mut order = Vec::new();
        while let Some((_, moved)) = p.pop_due(7) {
            order.push(moved);
        }
        assert_eq!(
            order,
            alloc::vec![10, 20, 30],
            "un plazo compartido reordena las peticiones"
        );
    }

    #[test]
    fn nothing_comes_out_before_it_is_due() {
        let w = AtomicUsize::new(0);
        let mut p = Pending::default();
        p.push(
            100,
            request(OpCode::Read, EntryType::Offset(0), &[], &w),
            BLKSIZE,
        );
        assert!(
            p.pop_due(99).is_none(),
            "ha salido un tick antes de su plazo"
        );
        assert!(p.pop_due(100).is_some(), "no ha salido en su plazo exacto");
        assert_eq!(p.len(), 0);
    }

    #[test]
    fn a_request_still_out_wakes_nobody_and_stays_in_the_queue() {
        let mut d = disk(alloc::vec![0u8; BLKSIZE]);
        let w = AtomicUsize::new(0);
        let buf = alloc::vec![9u8; BLKSIZE];
        let mut entries = [request(OpCode::Write, EntryType::Offset(0), &buf, &w)];
        // Empezada en el tick 100 y copiada por el 150: 50 ns de copia, que el
        // modelo cobra diez veces, asi que vence en 150 + 500.
        for entry in entries.iter_mut() {
            let mut reads = 0u32;
            d.accept(entry, || {
                reads += 1;
                if reads == 1 {
                    100
                } else {
                    150
                }
            });
        }
        let mut woken = Vec::new();
        d.deliver(649, |c| woken.push(c));
        assert!(woken.is_empty(), "ha avisado antes de su plazo");
        assert_eq!(decode_done(w.load(Ordering::Acquire)), None);
        d.deliver(650, |c| woken.push(c));
        assert_eq!(woken.len(), 1, "no ha avisado en su plazo");
        assert_eq!(decode_done(w.load(Ordering::Acquire)), Some(BLKSIZE));
    }

    #[test]
    fn the_deadline_charges_the_copy_ten_times_over() {
        assert_eq!(
            finish_time(100, 110),
            210,
            "10 ns de copia son 100 ns de disco"
        );
        assert_eq!(finish_time(5, 5), 5, "una copia que no se ve vence ya");
        assert_eq!(finish_time(0, 0), 0);
    }

    #[test]
    fn a_clock_that_reads_backwards_does_not_put_the_deadline_out_of_reach() {
        // `timer_now` no es monotono en todas las rutas: x86_64 solo pone suelo
        // cuando el TSC no es invariante, y aarch64 lee CNTVCT_EL0 a pelo. Una
        // resta sin proteger se sale de u128 por abajo y deja un plazo al que
        // `now` no llega jamas, o sea la misma CPU colgada.
        let due = finish_time(200, 100);
        assert_eq!(due, 100, "un reloj hacia atras ha movido el plazo");
        assert!(
            due <= u64::MAX as u128,
            "el plazo {} no lo alcanza ninguna maquina",
            due
        );
    }

    #[test]
    fn a_completion_word_of_zero_means_the_request_is_still_out() {
        assert_eq!(decode_done(0), None);
    }

    #[test]
    fn a_request_the_image_refuses_still_wakes_its_client() {
        assert_ne!(
            encode_done(0),
            0,
            "cero es el centinela que el cliente espera: un cero no despierta a nadie"
        );
        assert_eq!(decode_done(encode_done(0)), Some(0));
    }

    #[test]
    fn every_byte_count_survives_the_completion_word() {
        for moved in [0, 1, 511, BLKSIZE, 4096, 1 << 20] {
            assert_eq!(
                decode_done(encode_done(moved)),
                Some(moved),
                "{moved} bytes no vuelven enteros"
            );
        }
    }

    #[test]
    fn a_request_past_the_end_of_the_image_is_refused_and_not_asserted() {
        assert_eq!(
            request_range(4096 - 8, 16, 4096),
            None,
            "se sale por 8 bytes"
        );
        assert_eq!(request_range(4096, 1, 4096), None, "empieza justo detras");
    }

    #[test]
    fn a_length_that_runs_off_the_top_of_usize_is_refused() {
        // Sin `checked_add`, `start + len` da la vuelta y cae en un `end`
        // pequeno que cualquier `end <= cap` deja pasar.
        assert_eq!(request_range(usize::MAX - 3, 8, 4096), None);
        assert_eq!(request_range(usize::MAX, usize::MAX, 4096), None);
    }

    #[test]
    fn a_request_that_ends_exactly_at_the_end_of_the_image_fits() {
        assert_eq!(
            request_range(4096 - BLKSIZE, BLKSIZE, 4096),
            Some(3584..4096),
            "el ultimo bloque de la imagen es legible"
        );
        assert_eq!(request_range(0, 4096, 4096), Some(0..4096));
        assert_eq!(request_range(0, 0, 0), Some(0..0));
    }

    #[test]
    fn the_count_the_disk_reports_is_the_one_it_moved_and_not_a_block() {
        // SFS pide 4096 bytes de una vez. La palabra de aviso llevaba `BLKSIZE`
        // fuera cual fuera el tamano de la peticion, y lo unico que la miraba la
        // comparaba contra ese mismo `BLKSIZE`: una constante contra una
        // constante.
        let mut d = disk(alloc::vec![0xab; 8192]);
        let w = AtomicUsize::new(0);
        let buf = alloc::vec![0u8; 4096];
        let mut entries = [request(OpCode::Read, EntryType::Offset(0), &buf, &w)];
        assert_eq!(run(&mut d, &mut entries, 0, 0).len(), 1);
        assert_eq!(
            decode_done(w.load(Ordering::Acquire)),
            Some(4096),
            "el disco dice haber movido otra cantidad de la que movio"
        );
        assert!(
            buf.iter().all(|&b| b == 0xab),
            "los 4096 bytes no han llegado al buffer del cliente"
        );
    }

    #[test]
    fn a_refused_request_and_a_served_one_in_the_same_batch_both_report_truthfully() {
        let mut d = disk(alloc::vec![0u8; 4096]);
        let (w_ok, w_no) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let good = alloc::vec![0x5a; BLKSIZE];
        let far = alloc::vec![0u8; BLKSIZE];
        let mut entries = [
            request(OpCode::Write, EntryType::Offset(0), &good, &w_ok),
            // Fuera de la imagen: lo que antes hacia `assert!` en el nucleo del
            // disco, llevandose el disco y con el todas las CPUs que esperaban.
            request(OpCode::Read, EntryType::Offset(1 << 40), &far, &w_no),
        ];
        assert_eq!(
            run(&mut d, &mut entries, 50, 50).len(),
            2,
            "una de las dos no ha recibido aviso"
        );
        assert_eq!(
            decode_done(w_ok.load(Ordering::Acquire)),
            Some(BLKSIZE),
            "la peticion buena no ha movido su bloque"
        );
        assert_eq!(
            decode_done(w_no.load(Ordering::Acquire)),
            Some(0),
            "la peticion fuera de la imagen tiene que volver con cero, no colgarse"
        );
        assert_eq!(d.data.0[0], 0x5a);
    }

    #[test]
    fn a_block_request_is_refused_unless_it_is_exactly_one_block() {
        let image: &'static mut [u8] = Box::leak(alloc::vec![7u8; BLKSIZE * 4].into_boxed_slice());
        let mb = MemBuf(image);
        let mut half = [0u8; BLKSIZE / 2];
        assert_eq!(
            mb.read_block(0, &mut half),
            0,
            "medio bloque no es un bloque"
        );
        let mut whole = [0u8; BLKSIZE];
        assert_eq!(
            mb.read_block(3, &mut whole),
            BLKSIZE,
            "el ultimo bloque existe"
        );
        assert!(whole.iter().all(|&b| b == 7));
        assert_eq!(
            mb.read_block(4, &mut whole),
            0,
            "el bloque 4 no cabe en una imagen de cuatro bloques"
        );
    }

    #[test]
    fn a_block_id_that_overflows_the_offset_is_refused() {
        let image: &'static mut [u8] = Box::leak(alloc::vec![0u8; BLKSIZE].into_boxed_slice());
        let mb = MemBuf(image);
        let mut buf = [0u8; BLKSIZE];
        assert_eq!(
            mb.read_block(usize::MAX / 8, &mut buf),
            0,
            "block_id * BLKSIZE se sale de usize y aterriza en un offset cualquiera"
        );
        // Y el que de verdad se cuela: 2^55 bloques de 512 bytes son 2^64
        // justos, o sea CERO al dar la vuelta, asi que una multiplicacion sin
        // comprobar sirve el bloque 0 y dice que ha servido el 2^55.
        assert_eq!(
            mb.read_block(1 << 55, &mut buf),
            0,
            "un block_id cuyo producto da la vuelta hasta cero ha leido el bloque 0"
        );
    }

    #[test]
    fn a_write_and_then_a_read_through_handle_entry_move_the_same_bytes() {
        let mut d = disk(alloc::vec![0u8; BLKSIZE * 4]);
        let w = AtomicUsize::new(0);
        let pattern: Vec<u8> = (0..BLKSIZE).map(|i| (i % 251) as u8).collect();
        let mut written = [request(OpCode::Write, EntryType::Block(2), &pattern, &w)];
        assert_eq!(run(&mut d, &mut written, 0, 0).len(), 1);
        assert_eq!(decode_done(w.load(Ordering::Acquire)), Some(BLKSIZE));

        let w2 = AtomicUsize::new(0);
        let back = alloc::vec![0u8; BLKSIZE];
        let mut read = [request(
            OpCode::Read,
            EntryType::Offset(2 * BLKSIZE),
            &back,
            &w2,
        )];
        assert_eq!(run(&mut d, &mut read, 0, 0).len(), 1);
        assert_eq!(decode_done(w2.load(Ordering::Acquire)), Some(BLKSIZE));
        assert_eq!(
            back, pattern,
            "lo leido por offset no es lo escrito por bloque"
        );

        // Y por el otro brazo de `handle_entry`, el que indexa por bloque: el
        // bloque 2 son los bytes 1024..1536, no los bytes 2..514.
        let w3 = AtomicUsize::new(0);
        let by_block = alloc::vec![0u8; BLKSIZE];
        let mut read_block = [request(OpCode::Read, EntryType::Block(2), &by_block, &w3)];
        assert_eq!(run(&mut d, &mut read_block, 0, 0).len(), 1);
        assert_eq!(decode_done(w3.load(Ordering::Acquire)), Some(BLKSIZE));
        assert_eq!(
            by_block, pattern,
            "el brazo por bloque ha leido de un sitio que no es el bloque 2"
        );
    }

    #[test]
    fn the_two_clock_readings_straddle_the_copy() {
        // Si las dos caen antes de copiar, toda copia mide cero, todo vence en
        // el acto y el disco simulado deja de ser once veces mas lento que la
        // memoria, que es lo unico que simula. El reloj mira aqui el buffer del
        // cliente: la primera lectura lo tiene que ver vacio y la segunda
        // lleno.
        let mut d = disk(alloc::vec![0x77; BLKSIZE]);
        let w = AtomicUsize::new(0);
        let buf = alloc::vec![0u8; BLKSIZE];
        let mut entries = [request(OpCode::Read, EntryType::Offset(0), &buf, &w)];
        let mut seen = Vec::new();
        for entry in entries.iter_mut() {
            d.accept(entry, || {
                seen.push(buf[0]);
                0
            });
        }
        assert_eq!(
            seen,
            alloc::vec![0x00, 0x77],
            "las dos lecturas del reloj no abrazan la copia"
        );
    }

    #[test]
    fn a_flush_moves_nothing_and_does_not_panic() {
        let mut d = disk(alloc::vec![1u8; BLKSIZE]);
        let w = AtomicUsize::new(0);
        let buf = alloc::vec![0u8; BLKSIZE];
        let mut entries = [request(OpCode::Flush, EntryType::Offset(0), &buf, &w)];
        assert_eq!(
            run(&mut d, &mut entries, 0, 0).len(),
            1,
            "un flush tambien tiene que despertar a su cliente"
        );
        assert_eq!(decode_done(w.load(Ordering::Acquire)), Some(0));
        assert!(
            d.data.0.iter().all(|&b| b == 1),
            "un flush ha tocado la imagen"
        );
    }

    #[test]
    fn nothing_the_disk_core_runs_can_panic_on_purpose() {
        // Un panico en el nucleo del disco ocurre dentro de
        // `fn mocking(..) -> !`: el disco desaparece y todas las CPUs que le
        // esperan se quedan colgadas. Los que habia: el `unwrap` del `send_ipi`
        // y los cuatro `assert!` de los limites.
        let src = include_str!("mock.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for needle in ["unwrap(", "expect(", "assert!(", "assert_eq!("] {
            let hits = code
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| l.contains(needle))
                .count();
            assert_eq!(
                hits, 0,
                "{needle} sigue en la ruta del disco: un panico ahi cuelga a todas las CPUs"
            );
        }
    }

    #[test]
    fn a_submitted_entry_is_traced_before_it_is_published_and_not_after() {
        // Pasado `commit_entry` la ranura es del consumidor, y el `alloc_entry`
        // de otro productor puede dar la vuelta al mismo indice (`entry_at`
        // indexa `idx % len`), asi que una traza tomada despues imprime la
        // peticion de otra CPU.
        let src = include_str!("mock.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        let body = code
            .split_once("fn submit_entry(")
            .expect("submit_entry sigue en el fichero")
            .1;
        let trace = body.find("trace!").expect("la traza del envio");
        let commit = body.find("commit_entry(idx)").expect("la publicacion");
        assert!(
            trace < commit,
            "la traza lee la entrada despues de publicarla"
        );
    }
}
