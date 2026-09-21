use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicBool, Ordering};

use crate::builder::IoMapper;
use crate::bus::pci_drivers::PciDriver;
use crate::scheme::{BlockScheme, Scheme};
use crate::{Device, DeviceError, DeviceResult};
use pci::{PCIDevice, BAR};

use lock::Mutex;

use super::nvme_queue::*;

const SECTOR_SIZE: usize = 512;

pub struct NvmeInterface {
    name: String,

    admin_queue: Arc<Mutex<NvmeQueue<ProviderImpl>>>,

    io_queues: Vec<Arc<Mutex<NvmeQueue<ProviderImpl>>>>,

    bar: usize,

    /// Doorbell stride in bytes (4 << CAP.DSTRD).
    stride: usize,

    irq: usize,

    /// Capacity in 512-byte sectors.
    capacity: usize,

    /// log2 of the namespace LBA size (9 = 512B, 12 = 4KiB).
    lba_shift: u8,

    /// Per-command transfer cap in bytes: the controller's advertised MDTS
    /// (Identify Controller byte 77, in units of 2^n minimum-page-size pages;
    /// 0 = unlimited), further clamped by callers to the bounce-buffer size.
    /// Starts at one page — the always-legal minimum — until Identify runs.
    max_transfer: usize,

    /// Set when a command timed out and never completed, so the controller may
    /// still DMA into the queue's shared bounce buffer at any moment. Every
    /// subsequent request is refused rather than risk handing back data the
    /// controller overwrote. See `submit_sync`.
    poisoned: AtomicBool,
}

/// Outcome of waiting for one specific completion.
enum CqWait {
    /// The awaited completion arrived with a success status; carries CQE dword 0.
    Done(u32),
    /// The awaited completion arrived reporting an error status. The command is
    /// finished, so its data buffer is no longer under controller ownership and
    /// re-issuing it is safe.
    Failed,
    /// Nothing arrived before the deadline. The command is STILL owned by the
    /// controller and its PRPs still point at the queue's bounce buffer, so that
    /// buffer must not be reused until the completion is seen.
    Timeout,
    /// CSTS.CFS: the controller reported a fatal status. It will not complete
    /// anything, so waiting longer is pointless — but its DMA engine is in an
    /// undefined state, so the buffer is no safer than after a plain timeout.
    Fatal,
}

impl NvmeInterface {
    const ADMIN_TIMEOUT_US: u64 = 5_000_000;
    const IO_TIMEOUT_US: u64 = 5_000_000;
    /// Extra grace given to a timed-out command purely to observe its
    /// completion, so we know the controller has stopped writing into the
    /// bounce buffer. See `submit_sync`.
    const DRAIN_TIMEOUT_US: u64 = 20_000_000;
    /// Floor for the hard spin cap. The cap is a safety net for a stopped
    /// timer, NOT a second timeout: the old flat 50 M spins cost roughly 2-3 s
    /// at 3 GHz (each spin does a clflush + mfence + volatile CQE read), i.e. it
    /// fired *before* the nominal 5 s timeout on a fast machine and after it on
    /// a slow one, making the effective timeout depend on CPU frequency. Scaled
    /// with the requested timeout and logged distinctly, mirroring the AHCI
    /// `wait_until` helper.
    const CQ_WAIT_MIN_SPINS: u64 = 500_000_000;
    /// Retries for an I/O command that came back with an *error completion*.
    /// Matching the AHCI driver, where a single transient hiccup used to abort
    /// a whole `apk` extraction with EIO. Only error completions are retried;
    /// a timed-out command is never re-issued (see `submit_sync`).
    const IO_RETRIES: u32 = 4;
    /// Settle time between I/O retries.
    const IO_RETRY_SETTLE_US: u64 = 50_000;

    pub fn new(bar: usize, irq: usize) -> DeviceResult<NvmeInterface> {
        // Controller Capabilities: doorbell stride, max queue entries, ready timeout
        let cap = unsafe { read_volatile(bar as *const u64) };
        let dstrd = ((cap >> 32) & 0xf) as u32;
        let stride = (4usize) << dstrd;
        let mqes = (cap & 0xffff) as usize + 1;
        // CAP.TO is in 500 ms units; keep at least 1 s as a floor.
        let ready_timeout_us = (((cap >> 24) & 0xff) as u64 * 500_000).max(1_000_000);
        warn!(
            "[nvme] CAP: {:#x}, DSTRD: {} (stride {}B), MQES: {}, TO: {}us",
            cap, dstrd, stride, mqes, ready_timeout_us
        );

        let admin_q_size = mqes.min(32);
        let io_q_size = mqes.min(128);

        let admin_queue = Arc::new(Mutex::new(NvmeQueue::new(0, admin_q_size)));
        let io_queues = vec![Arc::new(Mutex::new(NvmeQueue::<ProviderImpl>::new(
            1, io_q_size,
        )))];

        let mut interface = NvmeInterface {
            name: String::from("nvme"),
            admin_queue,
            io_queues,
            bar,
            stride,
            irq,
            capacity: 0,
            lba_shift: 9,
            max_transfer: PAGE_SIZE,
            poisoned: AtomicBool::new(false),
        };

        interface.nvme_configure_admin_queue(ready_timeout_us)?;
        interface.nvme_alloc_io_queue()?;

        Ok(interface)
    }

    pub fn get_name_irq(&self) -> (String, usize) {
        (self.name.clone(), self.irq)
    }
}

fn clflush_range(vaddr: usize, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::{_mm_clflush, _mm_mfence};
        let line_size = 64;
        let start = vaddr & !(line_size - 1);
        let end = (vaddr + len + line_size - 1) & !(line_size - 1);
        unsafe {
            _mm_mfence();
            for addr in (start..end).step_by(line_size) {
                _mm_clflush(addr as *const u8);
            }
            _mm_mfence();
        }
    }
    // Non-x86 cache maintenance for non-coherent DMA is a TODO; rely on coherent
    // mappings for now.
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (vaddr, len);
}

impl NvmeInterface {
    /// Poll `queue`'s completion queue until the CQE for `cid` is consumed or
    /// `timeout_us` elapses.
    ///
    /// Handles phase tracking and the CQ head doorbell, which is updated for
    /// every consumed entry — including error and stale completions — so the
    /// queue never gets out of sync with the controller.
    fn wait_cq(
        bar: usize,
        cq_db: usize,
        queue: &mut NvmeQueue<ProviderImpl>,
        cid: u16,
        timeout_us: u64,
        context: &str,
    ) -> CqWait {
        let start = timer_now_as_micros();
        let mut spins = 0_u64;
        // Safety net for a timer that does not advance, not a second timeout:
        // large enough that the timer-based check below always wins on a
        // healthy machine. See `CQ_WAIT_MIN_SPINS`.
        let max_spins = Self::CQ_WAIT_MIN_SPINS.max(timeout_us.saturating_mul(500));

        loop {
            let head = queue.cq_head;
            clflush_range(&queue.cq[head] as *const _ as usize, 16);
            let entry = queue.cq[head].read();

            if (entry.status & 1) as usize == queue.cq_phase {
                queue.cq_head += 1;
                if queue.cq_head >= queue.cq.len() {
                    queue.cq_head = 0;
                    queue.cq_phase ^= 1;
                }
                unsafe { write_volatile(cq_db as *mut u32, queue.cq_head as u32) }

                if entry.command_id != cid {
                    // A completion for a *different* CID than the one we're
                    // awaiting: the command we want may still be in flight, so
                    // consume this stale CQE (head/phase/doorbell already
                    // advanced) and keep polling instead of abandoning ours —
                    // returning here desynced and could wedge the queue. Still
                    // bounded by the timeout / CFS checks below.
                    warn!(
                        "[nvme] stale completion cid {:#x} (wanted {:#x}) for {}; continuing",
                        entry.command_id, cid, context
                    );
                    continue;
                }

                let sc = (entry.status >> 1) & 0xff;
                let sct = (entry.status >> 9) & 0x7;
                if sc != 0 || sct != 0 {
                    warn!(
                        "[nvme] completion error: status={:#x} (sct={}, sc={}) for {}",
                        entry.status, sct, sc, context
                    );
                    return CqWait::Failed;
                }
                return CqWait::Done(entry.result as u32);
            }

            core::hint::spin_loop();
            spins = spins.saturating_add(1);

            if spins.is_multiple_of(256) {
                let csts = unsafe { read_volatile((bar + NVME_REG_CSTS) as *const u32) };
                if csts & NVME_CSTS_CFS != 0 {
                    warn!(
                        "[nvme] controller fatal status while waiting for {}",
                        context
                    );
                    return CqWait::Fatal;
                }
            }

            if spins >= max_spins {
                warn!(
                    "[nvme] CQ{} wait fallback: timer stuck, giving up after {} spins for {}",
                    queue.qid, spins, context
                );
                return CqWait::Timeout;
            }

            if timer_now_as_micros().wrapping_sub(start) >= timeout_us {
                warn!(
                    "[nvme] timeout waiting CQ{} completion (head={}, phase={}) for {}",
                    queue.qid, head, queue.cq_phase, context
                );
                return CqWait::Timeout;
            }
        }
    }

    /// Submit one command on `queue` and poll its completion queue until done.
    ///
    /// Returns CQE dword 0 (command result) on success.
    ///
    /// On a timeout the command is still owned by the controller, and its PRPs
    /// still point at `queue`'s bounce buffer — the single buffer every read and
    /// write on that queue shares. Returning straight away (what the old code
    /// did) left the next transfer's data exposed: if the abandoned command
    /// landed later, its DMA overwrote the new data and the read returned
    /// another sector's contents with no error anywhere. So we first spend
    /// `DRAIN_TIMEOUT_US` doing nothing but waiting for that completion to
    /// appear, which is what tells us the controller has stopped writing. If it
    /// never appears, the queue's buffer can be clobbered at any moment, so the
    /// device is marked poisoned and every later request is refused instead of
    /// silently returning corrupt data. Recovering properly from that state
    /// needs a controller reset (CC.EN=0, re-enable, recreate the I/O queues),
    /// which this driver does not implement yet.
    fn submit_sync(
        bar: usize,
        stride: usize,
        queue: &mut NvmeQueue<ProviderImpl>,
        mut cmd: NvmeCommonCommand,
        timeout_us: u64,
        context: &str,
        poisoned: &AtomicBool,
    ) -> DeviceResult<u32> {
        if poisoned.load(Ordering::Relaxed) {
            return Err(DeviceError::IoError);
        }

        let sq_db = bar + NVME_REG_DBS + 2 * queue.qid * stride;
        let cq_db = sq_db + stride;

        let cid = queue.next_cid();
        cmd.command_id = cid;

        let tail = queue.sq_tail;
        queue.sq[tail].write(cmd);
        queue.sq_tail = if tail + 1 >= queue.sq.len() {
            0
        } else {
            tail + 1
        };

        // Make the SQ entry visible to the device before ringing the doorbell.
        clflush_range(&queue.sq[tail] as *const _ as usize, 64);
        fence(Ordering::SeqCst);
        unsafe { write_volatile(sq_db as *mut u32, queue.sq_tail as u32) }

        let outcome = match Self::wait_cq(bar, cq_db, queue, cid, timeout_us, context) {
            CqWait::Done(result) => return Ok(result),
            CqWait::Failed => return Err(DeviceError::IoError),
            // A fatal controller status will never produce a completion, so
            // draining would only burn the whole grace period before reaching
            // the same conclusion.
            CqWait::Fatal => CqWait::Fatal,
            CqWait::Timeout => {
                warn!(
                    "[nvme] draining timed-out command cid {:#x} ({}) before reusing its buffer",
                    cid, context
                );
                Self::wait_cq(bar, cq_db, queue, cid, Self::DRAIN_TIMEOUT_US, context)
            }
        };

        match outcome {
            // The command finished after all: the controller is no longer
            // touching the bounce buffer, so the queue stays usable.
            CqWait::Done(_) | CqWait::Failed => Err(DeviceError::IoError),
            CqWait::Timeout | CqWait::Fatal => {
                poisoned.store(true, Ordering::Relaxed);
                error!(
                    "[nvme] command cid {:#x} ({}) never completed; the controller may still DMA \
                     into the shared transfer buffer. Disabling this device rather than return \
                     data it can overwrite — a reboot is needed to recover it.",
                    cid, context
                );
                Err(DeviceError::IoError)
            }
        }
    }

    pub fn nvme_configure_admin_queue(&mut self, ready_timeout_us: u64) -> DeviceResult {
        let bar = self.bar;
        let stride = self.stride;
        let admin = self.admin_queue.clone();
        let mut admin_queue = admin.lock();

        // Reset controller first
        warn!("[nvme] Resetting controller...");
        unsafe {
            let cc = read_volatile((bar + NVME_REG_CC) as *const u32);
            write_volatile((bar + NVME_REG_CC) as *mut u32, cc & !NVME_CC_ENABLE);
        }

        // Wait for CSTS.RDY to become 0
        let start = timer_now_as_micros();
        loop {
            let csts = unsafe { read_volatile((bar + NVME_REG_CSTS) as *const u32) };
            if (csts & NVME_CSTS_RDY) == 0 {
                break;
            }
            if timer_now_as_micros().wrapping_sub(start) > ready_timeout_us {
                warn!("[nvme] timeout waiting for controller reset");
                return Err(DeviceError::IoError);
            }
            core::hint::spin_loop();
        }

        // Admin queue attributes: 0-based sizes for SQ (bits 0-11) and CQ (bits 16-27)
        let aqa_entries = (admin_queue.sq.len() - 1) as u32;
        let aqa = (aqa_entries << 16) | aqa_entries;

        unsafe {
            write_volatile((bar + NVME_REG_AQA) as *mut u32, aqa);
            write_volatile((bar + NVME_REG_ASQ) as *mut u64, admin_queue.sq_pa as u64);
            write_volatile((bar + NVME_REG_ACQ) as *mut u64, admin_queue.cq_pa as u64);
        }

        // enable ctrl
        let mut ctrl_config = NVME_CC_ENABLE | NVME_CC_CSS_NVM;
        ctrl_config |= 0 << NVME_CC_MPS_SHIFT;
        ctrl_config |= NVME_CC_ARB_RR | NVME_CC_SHN_NONE;
        ctrl_config |= NVME_CC_IOSQES | NVME_CC_IOCQES;

        unsafe { write_volatile((bar + NVME_REG_CC) as *mut u32, ctrl_config) }

        // Wait for CSTS.RDY to become 1
        let start = timer_now_as_micros();
        loop {
            let csts = unsafe { read_volatile((bar + NVME_REG_CSTS) as *const u32) };
            if (csts & NVME_CSTS_RDY) != 0 {
                break;
            }
            if csts & NVME_CSTS_CFS != 0 {
                warn!("[nvme] controller fatal status during enable");
                return Err(DeviceError::IoError);
            }
            if timer_now_as_micros().wrapping_sub(start) > ready_timeout_us {
                warn!("[nvme] timeout waiting for controller ready");
                return Err(DeviceError::IoError);
            }
            core::hint::spin_loop();
        }
        warn!("[nvme] Controller ready!");

        // We poll for completions; mask all controller interrupts.
        unsafe { write_volatile((bar + NVME_REG_INTMS) as *mut u32, 0xffff_ffff) }

        let data_va = admin_queue.data_va;
        let data_pa = admin_queue.data_pa;

        // Identify Controller (CNS = 1)
        clflush_range(data_va, 4096);
        let mut cmd = NvmeIdentify::new();
        cmd.prp1 = data_pa as u64;
        cmd.nsid = 0;
        cmd.cns = 1;
        let common_cmd = unsafe { core::mem::transmute(cmd) };
        Self::submit_sync(
            bar,
            stride,
            &mut admin_queue,
            common_cmd,
            Self::ADMIN_TIMEOUT_US,
            "identify controller",
            &self.poisoned,
        )?;
        clflush_range(data_va, 4096);

        // Model number: bytes 24..63 of the Identify Controller data (ASCII)
        let model = unsafe { core::slice::from_raw_parts((data_va + 24) as *const u8, 40) };
        if let Ok(model) = core::str::from_utf8(model) {
            warn!("[nvme] model: {}", model.trim());
        }

        // MDTS (byte 77): max data transfer size as 2^n minimum-page-size
        // pages, 0 = unlimited. CAP.MPSMIN is virtually always 4 KiB; using
        // that floor as the unit keeps our computed cap <= the controller's
        // real limit even when MPSMIN is larger. `io_rw` callers clamp every
        // command to this, so the 128 KiB bounce never exceeds what the
        // controller accepts.
        let mdts = unsafe { read_volatile((data_va + 77) as *const u8) };
        self.max_transfer = max_transfer_from_mdts(mdts);
        warn!(
            "[nvme] MDTS: {} ({} per command)",
            mdts,
            if self.max_transfer == usize::MAX {
                String::from("unlimited")
            } else {
                alloc::format!("{} KiB", self.max_transfer / 1024)
            }
        );

        // Identify Namespace 1 (CNS = 0)
        clflush_range(data_va, 4096);
        let mut cmd = NvmeIdentify::new();
        cmd.cns = 0;
        cmd.prp1 = data_pa as u64;
        cmd.nsid = 1;
        let common_cmd = unsafe { core::mem::transmute(cmd) };
        Self::submit_sync(
            bar,
            stride,
            &mut admin_queue,
            common_cmd,
            Self::ADMIN_TIMEOUT_US,
            "identify namespace",
            &self.poisoned,
        )?;
        clflush_range(data_va, 4096);

        // Namespace size (LBAs) and current LBA format
        let nsze = unsafe { read_volatile(data_va as *const u64) };
        let flbas = unsafe { read_volatile((data_va + 26) as *const u8) };
        let lbaf_index = (flbas & 0xF) as usize;
        let lbaf_offset = 128 + lbaf_index * 4;
        let lbaf = unsafe { read_volatile((data_va + lbaf_offset) as *const u32) };
        let lbads = ((lbaf >> 16) & 0xFF) as u8;

        drop(admin_queue);

        if nsze == 0 {
            warn!("[nvme] namespace 1 has zero size, not usable");
            return Err(DeviceError::NoResources);
        }
        // Cap LBA size at 8 KiB: the single-LBA (partial-update) path assumes
        // one LBA always fits the bounce comfortably, and no real consumer
        // formats beyond 4 KiB anyway.
        if !lba_shift_supported(lbads) {
            warn!("[nvme] unsupported LBA size 2^{} bytes", lbads);
            return Err(DeviceError::NotSupported);
        }

        self.lba_shift = lbads;
        self.capacity = capacity_in_sectors(nsze, lbads);
        warn!(
            "[nvme] namespace 1: {} LBAs of {}B, capacity {} sectors (512B)",
            nsze,
            1u32 << lbads,
            self.capacity
        );

        Ok(())
    }

    pub fn nvme_alloc_io_queue(&mut self) -> DeviceResult {
        let bar = self.bar;
        let stride = self.stride;
        let mut admin_queue = self.admin_queue.lock();
        let io_queue = self.io_queues[0].lock();

        // Set Features: Number of Queues (request 1 IO SQ + 1 IO CQ, 0-based)
        let mut cmd = NvmeCommonCommand::new();
        cmd.opcode = 0x09;
        cmd.cdw10 = NVME_FEAT_NUM_QUEUES;
        cmd.cdw11 = 0;
        let result = Self::submit_sync(
            bar,
            stride,
            &mut admin_queue,
            cmd,
            Self::ADMIN_TIMEOUT_US,
            "set queue count",
            &self.poisoned,
        )?;
        trace!(
            "[nvme] controller allocated {} IO SQs / {} IO CQs",
            (result & 0xffff) + 1,
            (result >> 16) + 1
        );

        // Create IO Completion Queue (qid 1). We poll, so no interrupts.
        let mut cmd = NvmeCreateCq::new();
        cmd.prp1 = io_queue.cq_pa as u64;
        cmd.cqid = 1;
        cmd.qsize = (io_queue.cq.len() - 1) as u16;
        cmd.cq_flags = NVME_QUEUE_PHYS_CONTIG;
        let common_cmd = unsafe { core::mem::transmute(cmd) };
        Self::submit_sync(
            bar,
            stride,
            &mut admin_queue,
            common_cmd,
            Self::ADMIN_TIMEOUT_US,
            "create io completion queue",
            &self.poisoned,
        )?;

        // Create IO Submission Queue (qid 1, bound to CQ 1)
        let mut cmd = NvmeCreateSq::new();
        cmd.prp1 = io_queue.sq_pa as u64;
        cmd.sqid = 1;
        cmd.qsize = (io_queue.sq.len() - 1) as u16;
        cmd.sq_flags = NVME_QUEUE_PHYS_CONTIG;
        cmd.cqid = 1;
        let common_cmd = unsafe { core::mem::transmute(cmd) };
        Self::submit_sync(
            bar,
            stride,
            &mut admin_queue,
            common_cmd,
            Self::ADMIN_TIMEOUT_US,
            "create io submission queue",
            &self.poisoned,
        )?;

        Ok(())
    }

    /// One read/write command on the IO queue, transferring `len` bytes
    /// through the queue's bounce buffer. PRP1 covers page 0; a two-page
    /// transfer points PRP2 at page 1 directly; anything larger points PRP2
    /// at the queue's PRP list holding the remaining page addresses (the
    /// standard NVMe mechanism — one list page covers 512 entries, far above
    /// the 31 the 128 KiB bounce needs).
    fn check_request(&self, block_id: usize, len: usize) -> DeviceResult {
        check_request(block_id, len, self.capacity)
    }

    fn io_rw(
        &self,
        queue: &mut NvmeQueue<ProviderImpl>,
        write: bool,
        slba: u64,
        nlb_minus_1: u16,
        len: usize,
    ) -> DeviceResult {
        let mut cmd = if write {
            NvmeRWCommand::new_write_command()
        } else {
            NvmeRWCommand::new_read_command()
        };
        cmd.nsid = 1;
        cmd.prp1 = queue.data_pa as u64;
        match prp2_for(queue.data_pa, len) {
            Prp2::Unused => {}
            Prp2::SecondPage(pa) => cmd.prp2 = pa,
            Prp2::List(entries) => {
                // The bounce is physically contiguous, so the list is a
                // simple arithmetic fill.
                for i in 0..entries {
                    unsafe {
                        write_volatile(
                            (queue.prp_list_va as *mut u64).add(i),
                            prp_list_entry(queue.data_pa, i),
                        );
                    }
                }
                clflush_range(queue.prp_list_va, entries * 8);
                cmd.prp2 = queue.prp_list_pa as u64;
            }
        }
        cmd.slba = slba;
        cmd.length = nlb_minus_1;

        let common_cmd: NvmeCommonCommand = unsafe { core::mem::transmute(cmd) };
        let context = if write { "write block" } else { "read block" };

        // Retry an error completion, as the AHCI driver does: a single transient
        // hiccup should not fail the caller's whole operation. Safe because an
        // error completion means the command is finished, so the bounce buffer
        // is ours again and re-issuing the identical command is idempotent —
        // its contents (for a write) and its PRPs are untouched. A *timed-out*
        // command is never retried: `submit_sync` either drains it or poisons
        // the device, and a poisoned device short-circuits below.
        let mut attempt = 1u32;
        loop {
            match Self::submit_sync(
                self.bar,
                self.stride,
                queue,
                common_cmd,
                Self::IO_TIMEOUT_US,
                context,
                &self.poisoned,
            ) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    if attempt >= Self::IO_RETRIES || self.poisoned.load(Ordering::Relaxed) {
                        return Err(e);
                    }
                    attempt += 1;
                    warn!(
                        "[nvme] retrying {} at lba {:#x} (attempt {}/{})",
                        context,
                        slba,
                        attempt,
                        Self::IO_RETRIES
                    );
                    udelay(Self::IO_RETRY_SETTLE_US);
                }
            }
        }
    }
}

/// Where PRP2 points for a transfer of `len` bytes out of a physically
/// contiguous, page-aligned buffer.
///
/// NVMe describes a data buffer with two fields and, past two pages, a list.
/// PRP1 always covers the first page. What PRP2 means then depends on the
/// size, and the three cases are not interchangeable: point it at a page
/// when the controller expects a list and it reads eight bytes of data as
/// addresses, scattering the transfer across whatever those bytes happen to
/// spell.
#[derive(Debug, PartialEq, Eq)]
enum Prp2 {
    /// One page or less. The field is not read at all.
    Unused,
    /// Exactly two pages. PRP2 is the second page itself.
    SecondPage(u64),
    /// More than two. PRP2 points at a list holding this many entries, one
    /// per page after the first.
    List(usize),
}

fn prp2_for(data_pa: usize, len: usize) -> Prp2 {
    if len > PAGE_SIZE * 2 {
        // Page 0 rides in PRP1; the list covers the rest.
        Prp2::List(len.div_ceil(PAGE_SIZE) - 1)
    } else if len > PAGE_SIZE {
        Prp2::SecondPage((data_pa + PAGE_SIZE) as u64)
    } else {
        Prp2::Unused
    }
}

/// Entry `i` of the PRP list covers page `i + 1` of the buffer, because page
/// 0 is already in PRP1. Off by one here and every transfer larger than two
/// pages reads or writes one page too early.
fn prp_list_entry(data_pa: usize, i: usize) -> u64 {
    (data_pa + (i + 1) * PAGE_SIZE) as u64
}

/// How much of the caller's request the next command moves.
struct Chunk {
    /// Bytes the command transfers, always whole LBAs.
    io_len: usize,
    /// Bytes of it the caller actually gets, which is less when the request
    /// starts or ends inside an LBA.
    take: usize,
    /// The command's NLB field, which is **zero-based**: one LBA is spelled
    /// 0. Writing the count itself transfers one LBA too many, over the top
    /// of whatever follows.
    nlb: u16,
}

/// Split the next command out of a request, given how much is left, how far
/// into an LBA it starts, and the per-command cap.
///
/// `io_len` is never zero: the transfer loops advance by `take` and would
/// otherwise spin for ever, and `nlb` would be computed from a count of 0.
/// Today that is guaranteed by `lba_shift_supported` capping an LBA at 8 KiB
/// while `max_transfer_from_mdts` never returns less than that -- a pairing
/// that is easy to break from either side.
fn io_chunk(remaining: usize, off: usize, lba_bytes: usize, chunk: usize) -> Chunk {
    if off == 0 && remaining >= lba_bytes {
        let n = (remaining / lba_bytes).min(chunk / lba_bytes).max(1);
        Chunk {
            io_len: n * lba_bytes,
            take: n * lba_bytes,
            nlb: (n - 1) as u16,
        }
    } else {
        Chunk {
            io_len: lba_bytes,
            take: remaining.min(lba_bytes - off),
            nlb: 0,
        }
    }
}

/// Per-command transfer cap from Identify Controller byte 77.
///
/// MDTS is a power of two in minimum-page-size units, and 0 means no limit
/// at all -- not "no transfers". CAP.MPSMIN is virtually always 4 KiB; using
/// that floor as the unit keeps the computed cap at or below the
/// controller's real limit even when MPSMIN is larger.
///
/// `checked_shl` alone is not enough here: it checks the *shift amount*
/// against the word width, not whether the value survives. `PAGE_SIZE` is
/// 2^12, so any MDTS from 52 up shifts every bit off the top and comes back
/// as `Some(0)` -- a cap of zero bytes, which stalls the transfer loop and
/// underflows the block count. A shift that large is the controller saying
/// it has no practical limit, so it saturates like the 64-and-over case
/// already did.
fn max_transfer_from_mdts(mdts: u8) -> usize {
    if mdts == 0 {
        return usize::MAX;
    }
    match PAGE_SIZE.checked_shl(mdts as u32) {
        Some(n) if n >= PAGE_SIZE => n,
        _ => usize::MAX,
    }
}

/// LBA sizes this driver will work with, as the exponent in LBAF.LBADS.
///
/// The partial-update path reads one whole LBA into the bounce buffer, so an
/// LBA has to fit there comfortably; 8 KiB is the cap and no real consumer
/// formats beyond 4 KiB anyway. Below 512 bytes the 512-byte block API above
/// could not address a whole LBA.
fn lba_shift_supported(lbads: u8) -> bool {
    (9..=13).contains(&lbads)
}

/// Namespace size in the 512-byte units the `BlockScheme` API speaks. The
/// drive counts in its own LBAs, which are larger on anything but a 512-byte
/// format, so without the scaling the upper part of the disk disappears.
fn capacity_in_sectors(nsze: u64, lbads: u8) -> usize {
    (nsze as usize) << (lbads - 9)
}

/// Validate a request in 512-byte sectors against the namespace capacity,
/// mirroring the AHCI driver's `check_request`. Without this an out-of-range
/// request reached the controller and came back as a command error, which is
/// both slower and -- since the timeout path is the expensive one -- a
/// needless way to stress recovery. It also backstops a corrupt partition
/// table, whose `PartitionBlock` bounds are derived from on-disk data that
/// nothing else validates.
fn check_request(block_id: usize, len: usize, capacity: usize) -> DeviceResult {
    if len == 0 || !len.is_multiple_of(SECTOR_SIZE) {
        return Err(DeviceError::InvalidParam);
    }
    match block_id.checked_add(len / SECTOR_SIZE) {
        Some(end) if end <= capacity => Ok(()),
        _ => Err(DeviceError::InvalidParam),
    }
}

/// Busy-wait for `us` microseconds, with a hard spin cap so a stopped timer
/// cannot turn this into an infinite loop.
fn udelay(us: u64) {
    let t0 = timer_now_as_micros();
    const MAX_SPINS: u64 = 10_000_000;
    let mut spins = 0u64;
    while timer_now_as_micros().wrapping_sub(t0) < us {
        core::hint::spin_loop();
        spins = spins.wrapping_add(1);
        if spins >= MAX_SPINS {
            break;
        }
    }
}

impl BlockScheme for NvmeInterface {
    // `block_id` indexes 512-byte sectors (same convention as the AHCI
    // driver); `buf.len()` may be any multiple of 512.
    fn read_block(&self, block_id: usize, read_buf: &mut [u8]) -> DeviceResult {
        self.check_request(block_id, read_buf.len())?;
        let lba_bytes = 1usize << self.lba_shift;
        let mut queue = self.io_queues[0].lock();
        let queue = &mut *queue;

        let mut byte_addr = block_id * SECTOR_SIZE;
        let mut done = 0usize;
        while done < read_buf.len() {
            let remaining = read_buf.len() - done;
            let lba = (byte_addr / lba_bytes) as u64;
            let off = byte_addr % lba_bytes;

            // Whole-LBA transfers go in chunks of up to the bounce buffer size
            // (clamped to the controller's MDTS); a sector range inside a
            // bigger LBA reads the full LBA and copies out.
            let chunk = queue.data_len.min(self.max_transfer);
            let Chunk { io_len, take, nlb } = io_chunk(remaining, off, lba_bytes, chunk);

            clflush_range(queue.data_va, io_len);
            self.io_rw(queue, false, lba, nlb, io_len)?;
            clflush_range(queue.data_va, io_len);

            let src =
                unsafe { core::slice::from_raw_parts((queue.data_va + off) as *const u8, take) };
            read_buf[done..done + take].copy_from_slice(src);

            done += take;
            byte_addr += take;
        }
        Ok(())
    }

    fn write_block(&self, block_id: usize, write_buf: &[u8]) -> DeviceResult {
        self.check_request(block_id, write_buf.len())?;
        let lba_bytes = 1usize << self.lba_shift;
        let mut queue = self.io_queues[0].lock();
        let queue = &mut *queue;

        let mut byte_addr = block_id * SECTOR_SIZE;
        let mut done = 0usize;
        while done < write_buf.len() {
            let remaining = write_buf.len() - done;
            let lba = (byte_addr / lba_bytes) as u64;
            let off = byte_addr % lba_bytes;

            let take;
            if off == 0 && remaining >= lba_bytes {
                let chunk = queue.data_len.min(self.max_transfer);
                let Chunk { io_len, nlb, .. } = io_chunk(remaining, off, lba_bytes, chunk);
                take = io_len;

                let dst =
                    unsafe { core::slice::from_raw_parts_mut(queue.data_va as *mut u8, io_len) };
                dst.copy_from_slice(&write_buf[done..done + io_len]);
                clflush_range(queue.data_va, io_len);
                self.io_rw(queue, true, lba, nlb, io_len)?;
            } else {
                // Partial LBA update: read-modify-write through the bounce buffer.
                take = remaining.min(lba_bytes - off);

                clflush_range(queue.data_va, lba_bytes);
                self.io_rw(queue, false, lba, 0, lba_bytes)?;
                clflush_range(queue.data_va, lba_bytes);

                let dst = unsafe {
                    core::slice::from_raw_parts_mut((queue.data_va + off) as *mut u8, take)
                };
                dst.copy_from_slice(&write_buf[done..done + take]);
                clflush_range(queue.data_va, lba_bytes);
                self.io_rw(queue, true, lba, 0, lba_bytes)?;
            }

            done += take;
            byte_addr += take;
        }
        Ok(())
    }

    fn flush(&self) -> DeviceResult {
        let mut queue = self.io_queues[0].lock();
        let mut cmd = NvmeCommonCommand::new();
        cmd.opcode = 0x00; // Flush
        cmd.nsid = 1;
        // Retried like a data command: `apk` fsync()s after every extracted
        // file, and a flush is the command most likely to hit a transient
        // error because it can sit behind a large device write cache. Flush is
        // idempotent and carries no data buffer.
        let mut attempt = 1u32;
        loop {
            match Self::submit_sync(
                self.bar,
                self.stride,
                &mut queue,
                cmd,
                Self::IO_TIMEOUT_US,
                "flush",
                &self.poisoned,
            ) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    if attempt >= Self::IO_RETRIES || self.poisoned.load(Ordering::Relaxed) {
                        return Err(e);
                    }
                    attempt += 1;
                    warn!(
                        "[nvme] retrying flush (attempt {}/{})",
                        attempt,
                        Self::IO_RETRIES
                    );
                    udelay(Self::IO_RETRY_SETTLE_US);
                }
            }
        }
    }

    fn block_count(&self) -> usize {
        self.capacity
    }

    // The namespace's real LBA size, as chosen by its current LBA format
    // (Identify Namespace LBAF[FLBAS].LBADS). Reads and writes above translate
    // 512-byte `block_id`s onto it, but the partition tables on the disk are
    // laid out in *these* units, so the scanner needs the true value.
    fn logical_block_size(&self) -> usize {
        1usize << self.lba_shift
    }

    /// Flush the write cache, then perform the NVMe orderly shutdown (CC.SHN =
    /// normal) and wait — bounded — for CSTS.SHST to report completion. The
    /// spec expects hosts to do this before any reset; on a DRAM-less
    /// controller (e.g. SM2269XT) this is the moment the FTL mapping tables
    /// are persisted to NAND instead of being rebuilt on next power-up.
    fn quiesce_for_reboot(&self) {
        let _ = self.flush();
        unsafe {
            let cc = read_volatile((self.bar + NVME_REG_CC) as *const u32);
            write_volatile(
                (self.bar + NVME_REG_CC) as *mut u32,
                (cc & !(3 << 14)) | NVME_CC_SHN_NORMAL,
            );
        }
        let start = timer_now_as_micros();
        loop {
            let csts = unsafe { read_volatile((self.bar + NVME_REG_CSTS) as *const u32) };
            if csts & (3 << 2) == NVME_CSTS_SHST_CMPLT {
                break;
            }
            // RTD3 entry latency is typically well under a second; do not hold
            // the reboot hostage to a wedged controller.
            if timer_now_as_micros().wrapping_sub(start) > 1_000_000 {
                warn!("[nvme] shutdown handshake timed out; proceeding with reset");
                break;
            }
            core::hint::spin_loop();
        }
    }
}

impl Scheme for NvmeInterface {
    fn name(&self) -> &str {
        "nvme"
    }

    fn handle_irq(&self, irq: usize) {
        // Completions are polled; interrupts are masked via INTMS.
        trace!("nvme device irq {}", irq);
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
//64B
pub struct NvmeCommonCommand {
    opcode: u8,
    flags: u8,
    command_id: u16,
    nsid: u32,
    cdw2: [u32; 2],
    metadata: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

impl NvmeCommonCommand {
    pub fn new() -> Self {
        Self {
            opcode: 0,
            flags: 0,
            command_id: 0,
            nsid: 0,
            cdw2: [0; 2],
            metadata: 0,
            prp1: 0,
            prp2: 0,
            cdw10: 0,
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NvmeIdentify {
    opcode: u8,
    flags: u8,
    command_id: u16,
    nsid: u32,
    rsvd2: [u64; 2],
    prp1: u64,
    prp2: u64,
    cns: u8,
    rsvd3: u8,
    ctrlid: u16,
    rsvd11: [u8; 3],
    csi: u8,
    rsvd12: [u32; 4],
}

impl NvmeIdentify {
    pub fn new() -> Self {
        Self {
            opcode: 0x06,
            flags: 0,
            command_id: 0,
            nsid: 1,
            rsvd2: [0; 2],
            prp1: 0,
            prp2: 0,
            cns: 1,
            rsvd3: 0,
            ctrlid: 0,
            rsvd11: [0; 3],
            csi: 0,
            rsvd12: [0; 4],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NvmeCreateCq {
    pub opcode: u8,
    pub flags: u8,
    pub command_id: u16,
    pub nsid: u32,
    pub rsvd1: [u32; 4],
    pub prp1: u64,
    pub rsvd8: u64,
    pub cqid: u16,
    pub qsize: u16,
    pub cq_flags: u16,
    pub irq_vector: u16,
    pub rsvd12: [u32; 4],
}

impl NvmeCreateCq {
    fn new() -> Self {
        Self {
            opcode: 0x05,
            flags: 0,
            command_id: 0,
            nsid: 0,
            rsvd1: [0; 4],
            prp1: 0,
            rsvd8: 0,
            cqid: 0,
            qsize: 0,
            cq_flags: 0,
            irq_vector: 0,
            rsvd12: [0; 4],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NvmeCreateSq {
    pub opcode: u8,
    pub flags: u8,
    pub command_id: u16,
    pub nsid: u32,
    pub rsvd1: [u32; 4],
    pub prp1: u64,
    pub rsvd8: u64,
    pub sqid: u16,
    pub qsize: u16,
    pub sq_flags: u16,
    pub cqid: u16,
    pub rsvd12: [u32; 4],
}

impl NvmeCreateSq {
    fn new() -> Self {
        Self {
            opcode: 0x01,
            flags: 0,
            command_id: 0,
            nsid: 0,
            rsvd1: [0; 4],
            prp1: 0,
            rsvd8: 0,
            sqid: 0,
            qsize: 0,
            sq_flags: 0,
            cqid: 0,
            rsvd12: [0; 4],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct NvmeRWCommand {
    pub opcode: u8,
    pub flags: u8,
    pub command_id: u16,
    pub nsid: u32,
    pub rsvd2: u64,
    pub metadata: u64,
    pub prp1: u64,
    pub prp2: u64,
    pub slba: u64,
    pub length: u16,
    pub control: u16,
    pub dsmgmt: u32,
    pub reftag: u32,
    pub apptag: u16,
    pub appmask: u16,
}

impl NvmeRWCommand {
    pub fn new_write_command() -> Self {
        Self {
            opcode: 0x01,
            ..Default::default()
        }
    }
    pub fn new_read_command() -> Self {
        Self {
            opcode: 0x02,
            ..Default::default()
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct NvmeCompletion {
    pub result: u64,
    pub sq_head: u16,
    pub sq_id: u16,
    pub command_id: u16,
    pub status: u16,
}

// NvmeRegister
pub const NVME_REG_CAP: usize = 0x0000; /* Controller Capabilities */
pub const NVME_REG_VS: usize = 0x0008; /* Version */
pub const NVME_REG_INTMS: usize = 0x000c; /* Interrupt Mask Set */
pub const NVME_REG_INTMC: usize = 0x0010; /* Interrupt Mask Clear */
pub const NVME_REG_CC: usize = 0x0014; /* Controller Configuration */
pub const NVME_REG_CSTS: usize = 0x001c; /* Controller Status */
pub const NVME_REG_NSSR: usize = 0x0020; /* NVM Subsystem Reset */
pub const NVME_REG_AQA: usize = 0x0024; /* Admin Queue Attributes */
pub const NVME_REG_ASQ: usize = 0x0028; /* Admin SQ Base Address */
pub const NVME_REG_ACQ: usize = 0x0030; /* Admin CQ Base Address */
pub const NVME_REG_CMBLOC: usize = 0x0038; /* Controller Memory Buffer Location */
pub const NVME_REG_CMBSZ: usize = 0x003c; /* Controller Memory Buffer Size */
pub const NVME_REG_BPINFO: usize = 0x0040; /* Boot Partition Information */
pub const NVME_REG_BPRSEL: usize = 0x0044; /* Boot Partition Read Select */
pub const NVME_REG_BPMBL: usize = 0x0048; /* Boot Partition Memory Buffer
                                           * Location
                                           */
pub const NVME_REG_CMBMSC: usize = 0x0050; /* Controller Memory Buffer Memory
                                            * Space Control
                                            */
pub const NVME_REG_CRTO: usize = 0x0068; /* Controller Ready Timeouts */
pub const NVME_REG_PMRCAP: usize = 0x0e00; /* Persistent Memory Capabilities */
pub const NVME_REG_PMRCTL: usize = 0x0e04; /* Persistent Memory Region Control */
pub const NVME_REG_PMRSTS: usize = 0x0e08; /* Persistent Memory Region Status */
pub const NVME_REG_PMREBS: usize = 0x0e0c; /* Persistent Memory Region Elasticity
                                            * Buffer Size
                                            */
pub const NVME_REG_PMRSWTP: usize = 0x0e10; /* Persistent Memory Region Sustained
                                             * Write Throughput
                                             */
pub const NVME_REG_DBS: usize = 0x1000; /* SQ 0 Tail Doorbell */

// NVME CONST
pub const NVME_CC_ENABLE: u32 = 1 << 0;
pub const NVME_CC_CSS_NVM: u32 = 0 << 4;
pub const NVME_CC_MPS_SHIFT: u32 = 7;
pub const NVME_CC_ARB_RR: u32 = 0 << 11;
pub const NVME_CC_ARB_WRRU: u32 = 1 << 11;
pub const NVME_CC_ARB_VS: u32 = 7 << 11;
pub const NVME_CC_SHN_NONE: u32 = 0 << 14;
pub const NVME_CC_SHN_NORMAL: u32 = 1 << 14;
pub const NVME_CC_SHN_ABRUPT: u32 = 2 << 14;
pub const NVME_CC_IOSQES: u32 = 6 << 16;
pub const NVME_CC_IOCQES: u32 = 4 << 20;
pub const NVME_CSTS_RDY: u32 = 1 << 0;
pub const NVME_CSTS_CFS: u32 = 1 << 1;
pub const NVME_CSTS_SHST_NORMAL: u32 = 0 << 2;
pub const NVME_CSTS_SHST_OCCUR: u32 = 1 << 2;
pub const NVME_CSTS_SHST_CMPLT: u32 = 2 << 2;

pub const NVME_QUEUE_PHYS_CONTIG: u16 = 1 << 0;
pub const NVME_CQ_IRQ_ENABLED: u16 = 1 << 1;
pub const NVME_SQ_PRIO_URGENT: u16 = 0 << 1;
pub const NVME_SQ_PRIO_HIGH: u16 = 1 << 1;
pub const NVME_SQ_PRIO_MEDIUM: u16 = 2 << 1;
pub const NVME_SQ_PRIO_LOW: u16 = 3 << 1;

pub const NVME_FEAT_ARBITRATION: u32 = 0x01;
pub const NVME_FEAT_POWER_MGMT: u32 = 0x02;
pub const NVME_FEAT_LBA_RANGE: u32 = 0x03;
pub const NVME_FEAT_TEMP_THRESH: u32 = 0x04;
pub const NVME_FEAT_ERR_RECOVERY: u32 = 0x05;
pub const NVME_FEAT_VOLATILE_WC: u32 = 0x06;
pub const NVME_FEAT_NUM_QUEUES: u32 = 0x07;
pub const NVME_FEAT_IRQ_COALESCE: u32 = 0x08;
pub const NVME_FEAT_IRQ_CONFIG: u32 = 0x09;
pub const NVME_FEAT_WRITE_ATOMIC: u32 = 0x0a;
pub const NVME_FEAT_ASYNC_EVENT: u32 = 0x0b;
pub const NVME_FEAT_SW_PROGRESS: u32 = 0x0c;

pub struct NvmeDriverPci;

impl PciDriver for NvmeDriverPci {
    fn name(&self) -> &str {
        "nvme"
    }

    fn matched(&self, _vendor_id: u16, _device_id: u16) -> bool {
        false
    }

    fn matched_dev(&self, dev: &PCIDevice) -> bool {
        dev.id.class == 0x01 && dev.id.subclass == 0x08
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        if let Some(BAR::Memory(addr, _len, _, _)) = dev.bars[0] {
            if let Some(m) = mapper {
                m.query_or_map(addr as usize, 4096 * 8);
            }
            let vaddr = crate::bus::phys_to_virt(addr as usize);
            let vector = irq.map(|idx| idx + 32).unwrap_or(33);
            let blk = Arc::new(NvmeInterface::new(vaddr, vector)?);
            Ok(Device::Block(blk))
        } else {
            Err(crate::DeviceError::NotSupported)
        }
    }
}

#[cfg(test)]
mod command_tests {
    //! Host tests for the arithmetic behind an NVMe command.
    //!
    //! Like the AHCI driver next door, none of this runs in CI: eclipse boots
    //! in QEMU from an image in RAM and never issues a disk command, so the
    //! whole file is dead code there and alive on a real machine.
    //!
    //! What is checked is what the controller reads out of the submission
    //! queue entry. The two fields that decide where the data goes are PRP1
    //! and PRP2, and PRP2 means three different things depending on the
    //! size; the field that decides how much data moves is NLB, which is
    //! zero-based. Each of those is a way to read or write the wrong memory
    //! and have the command complete with no error.

    use super::*;

    // --- where the data goes ---

    #[test]
    fn a_transfer_of_one_page_or_less_does_not_use_the_second_pointer() {
        // PRP1 covers it all. Filling PRP2 anyway would be harmless on a
        // forgiving controller and a spec violation on a strict one.
        assert_eq!(prp2_for(0x20_0000, 512), Prp2::Unused);
        assert_eq!(prp2_for(0x20_0000, PAGE_SIZE), Prp2::Unused);
    }

    #[test]
    fn a_two_page_transfer_points_the_second_pointer_at_the_second_page() {
        // Not at a list: with only one page left there is nothing to list,
        // and the controller would read the data itself as addresses.
        assert_eq!(
            prp2_for(0x20_0000, PAGE_SIZE + 1),
            Prp2::SecondPage((0x20_0000 + PAGE_SIZE) as u64)
        );
        assert_eq!(
            prp2_for(0x20_0000, PAGE_SIZE * 2),
            Prp2::SecondPage((0x20_0000 + PAGE_SIZE) as u64)
        );
    }

    #[test]
    fn a_longer_transfer_switches_to_a_list_one_byte_later() {
        // The boundary is exactly two pages. One byte past it the meaning of
        // PRP2 changes completely, so this is where an off-by-one hides.
        assert_eq!(prp2_for(0x20_0000, PAGE_SIZE * 2 + 1), Prp2::List(2));
        assert_eq!(prp2_for(0x20_0000, PAGE_SIZE * 3), Prp2::List(2));
        assert_eq!(prp2_for(0x20_0000, PAGE_SIZE * 3 + 1), Prp2::List(3));
    }

    #[test]
    fn the_list_holds_one_entry_per_page_after_the_first() {
        // The whole 128 KiB bounce is 32 pages, so 31 entries. Counting the
        // first page twice would leave the last page of every large transfer
        // untransferred, silently.
        let pages = 32;
        assert_eq!(
            prp2_for(0x20_0000, PAGE_SIZE * pages),
            Prp2::List(pages - 1)
        );
        // A partial last page still needs its own entry.
        assert_eq!(
            prp2_for(0x20_0000, PAGE_SIZE * pages + 7),
            Prp2::List(pages)
        );
    }

    #[test]
    fn the_first_list_entry_is_the_second_page_not_the_first() {
        // Page 0 is already in PRP1. Starting the list at page 0 makes every
        // transfer over two pages read or write one page too early, which is
        // the classic way to scribble on the page before the buffer.
        let base = 0x20_0000;
        assert_eq!(prp_list_entry(base, 0), (base + PAGE_SIZE) as u64);
        assert_eq!(prp_list_entry(base, 1), (base + 2 * PAGE_SIZE) as u64);
        assert_eq!(prp_list_entry(base, 30), (base + 31 * PAGE_SIZE) as u64);
    }

    #[test]
    fn the_list_and_prp1_together_cover_the_whole_buffer_exactly_once() {
        // Walk the descriptors the way the controller does and check the
        // pages come out contiguous, in order, and with none repeated or
        // skipped.
        let base = 0x20_0000;
        for pages in 3..=32usize {
            let len = PAGE_SIZE * pages;
            let entries = match prp2_for(base, len) {
                Prp2::List(n) => n,
                other => panic!("{} pages should need a list, got {:?}", pages, other),
            };
            let mut seen = alloc::vec![base as u64];
            for i in 0..entries {
                seen.push(prp_list_entry(base, i));
            }
            assert_eq!(seen.len(), pages, "{} pages", pages);
            for (i, pa) in seen.iter().enumerate() {
                assert_eq!(*pa, (base + i * PAGE_SIZE) as u64, "{} pages", pages);
            }
        }
    }

    // --- how much data moves ---

    #[test]
    fn the_block_count_is_zero_based() {
        // NLB of 0 means one LBA. Sending the count itself transfers one LBA
        // too many, over the top of whatever follows the buffer.
        let c = io_chunk(512, 0, 512, 128 * 1024);
        assert_eq!((c.io_len, c.nlb), (512, 0));
        let c = io_chunk(4096, 0, 512, 128 * 1024);
        assert_eq!((c.io_len, c.nlb), (4096, 7));
    }

    #[test]
    fn a_whole_lba_transfer_hands_back_everything_it_moved() {
        let c = io_chunk(8192, 0, 4096, 128 * 1024);
        assert_eq!((c.io_len, c.take, c.nlb), (8192, 8192, 1));
    }

    #[test]
    fn the_command_is_capped_by_the_controllers_limit() {
        // MDTS is what the controller will accept; exceeding it is a command
        // error on every real device and accepted by most emulators.
        let c = io_chunk(1024 * 1024, 0, 512, 128 * 1024);
        assert_eq!(c.io_len, 128 * 1024);
        assert_eq!(c.nlb, (128 * 1024 / 512 - 1) as u16);
        let c = io_chunk(1024 * 1024, 0, 512, 8192);
        assert_eq!(c.io_len, 8192);
    }

    #[test]
    fn a_request_starting_inside_an_lba_reads_the_whole_lba_and_keeps_a_slice() {
        // A 4 KiB-formatted drive cannot address anything smaller. The
        // command has to be a whole LBA even though the caller wants 512
        // bytes out of the middle of it.
        let c = io_chunk(512, 1536, 4096, 128 * 1024);
        assert_eq!((c.io_len, c.take, c.nlb), (4096, 512, 0));
        // ...and never more of the LBA than is actually left in it.
        let c = io_chunk(8192, 3584, 4096, 128 * 1024);
        assert_eq!((c.io_len, c.take), (4096, 512));
    }

    #[test]
    fn a_request_ending_inside_an_lba_is_also_a_whole_lba_command() {
        let c = io_chunk(512, 0, 4096, 128 * 1024);
        assert_eq!((c.io_len, c.take, c.nlb), (4096, 512, 0));
    }

    #[test]
    fn a_command_always_moves_at_least_one_lba() {
        // io_len of zero would make the transfer loop spin for ever, and NLB
        // would be computed from a count of 0 -- an underflow that reads back
        // as 65536 blocks. Today the two bounds that prevent it are a cap on
        // the LBA size and a floor under MDTS, and at 8 KiB they meet exactly
        // with nothing to spare.
        for &lba_bytes in &[512usize, 1024, 2048, 4096, 8192] {
            for &chunk in &[512usize, 4096, 8192, 128 * 1024] {
                let c = io_chunk(1024 * 1024, 0, lba_bytes, chunk);
                assert_ne!(c.io_len, 0, "lba {} chunk {}", lba_bytes, chunk);
                assert_ne!(c.take, 0, "lba {} chunk {}", lba_bytes, chunk);
                assert_eq!(c.io_len % lba_bytes, 0, "lba {} chunk {}", lba_bytes, chunk);
                assert_eq!(c.io_len / lba_bytes, c.nlb as usize + 1);
            }
        }
    }

    #[test]
    fn every_command_is_a_whole_number_of_lbas() {
        for &lba_bytes in &[512usize, 4096] {
            for &remaining in &[1usize, 511, 512, 513, 4096, 4097, 100_000] {
                for &off in &[0usize, 1, 511, 512] {
                    if off >= lba_bytes {
                        continue;
                    }
                    let c = io_chunk(remaining, off, lba_bytes, 128 * 1024);
                    assert_eq!(c.io_len % lba_bytes, 0);
                    assert!(c.take <= c.io_len);
                    assert!(c.take <= remaining, "handed back more than was asked");
                    assert_ne!(c.take, 0, "the loop would not advance");
                }
            }
        }
    }

    // --- what the controller told us about itself ---

    #[test]
    fn an_mdts_of_zero_means_no_limit_not_no_transfers() {
        // Reading it as a size would cap every command at one byte and never
        // finish a read.
        assert_eq!(max_transfer_from_mdts(0), usize::MAX);
    }

    #[test]
    fn mdts_counts_pages_as_a_power_of_two() {
        assert_eq!(max_transfer_from_mdts(1), PAGE_SIZE * 2);
        assert_eq!(max_transfer_from_mdts(5), PAGE_SIZE * 32);
    }

    #[test]
    fn an_absurd_mdts_saturates_instead_of_wrapping_to_nothing() {
        // `checked_shl` guards the shift amount, not the value. A page is
        // 2^12, so from MDTS 52 upwards every bit falls off the top of a
        // 64-bit word and the "checked" answer is Some(0) -- a cap of zero
        // bytes. That stalls the transfer loop on the first read and
        // underflows the block count into 65536 blocks. Only MDTS 64 and
        // over hit the guard that was there.
        assert_eq!(max_transfer_from_mdts(255), usize::MAX);
        assert_eq!(max_transfer_from_mdts(64), usize::MAX);
        for mdts in 52..=63u8 {
            assert_eq!(
                max_transfer_from_mdts(mdts),
                usize::MAX,
                "MDTS {} came back as a cap of {} bytes",
                mdts,
                max_transfer_from_mdts(mdts)
            );
        }
        // Everything below the wrap is still its real value.
        assert_eq!(max_transfer_from_mdts(51), PAGE_SIZE << 51);
    }

    #[test]
    fn the_transfer_cap_never_comes_out_below_one_page() {
        // Whatever the controller reports, for every one of the 256 values
        // it can report. A cap under one page cannot describe even the
        // smallest command.
        for mdts in 0..=255u8 {
            assert!(
                max_transfer_from_mdts(mdts) >= PAGE_SIZE,
                "MDTS {} gave {}",
                mdts,
                max_transfer_from_mdts(mdts)
            );
        }
    }

    #[test]
    fn the_transfer_cap_is_never_smaller_than_the_largest_lba_we_accept() {
        // This is the pairing `io_chunk` depends on. If either side moves,
        // `chunk / lba_bytes` comes out zero and the driver hangs on the
        // first read of the root filesystem.
        let largest_lba = 1usize << 13;
        for mdts in 0..=8u8 {
            assert!(
                max_transfer_from_mdts(mdts) >= largest_lba,
                "mdts {} allows less than one LBA",
                mdts
            );
        }
    }

    #[test]
    fn only_lba_sizes_the_driver_can_actually_handle_are_accepted() {
        // Below 512 the 512-byte block API above cannot address a whole LBA;
        // above 8 KiB one LBA no longer fits the bounce buffer comfortably,
        // and the partial-update path reads a whole one.
        assert!(!lba_shift_supported(8));
        assert!(lba_shift_supported(9));
        assert!(lba_shift_supported(12));
        assert!(lba_shift_supported(13));
        assert!(!lba_shift_supported(14));
        assert!(!lba_shift_supported(0));
    }

    #[test]
    fn the_namespace_size_is_scaled_into_512_byte_sectors() {
        // The drive counts in its own LBAs. Taking the number at face value
        // on a 4 KiB drive makes seven eighths of the disk unreachable.
        assert_eq!(capacity_in_sectors(1000, 9), 1000);
        assert_eq!(capacity_in_sectors(1000, 12), 8000);
        assert_eq!(capacity_in_sectors(1000, 13), 16000);
    }

    // --- the request the filesystem hands us ---

    #[test]
    fn a_request_past_the_end_of_the_namespace_is_refused() {
        assert!(check_request(0, 512, 1000).is_ok());
        assert!(check_request(999, 512, 1000).is_ok());
        assert!(check_request(1000, 512, 1000).is_err());
        assert!(check_request(999, 1024, 1000).is_err());
    }

    #[test]
    fn a_request_that_is_not_a_whole_number_of_blocks_is_refused() {
        assert!(check_request(0, 0, 1000).is_err());
        assert!(check_request(0, 511, 1000).is_err());
        assert!(check_request(0, 513, 1000).is_err());
    }

    #[test]
    fn a_request_that_would_wrap_round_is_refused_rather_than_accepted() {
        // The sum overflowing would come out small and pass a naive bounds
        // check, aiming the transfer at the start of the namespace.
        assert!(check_request(usize::MAX, 512, usize::MAX).is_err());
        assert!(check_request(usize::MAX - 1, 4096, usize::MAX).is_err());
    }
}
