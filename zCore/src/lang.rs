// Rust language features implementations

use core::alloc::Layout;
use core::panic::PanicInfo;

#[cfg(not(test))]
#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    // The heap is exhausted here, so we must NOT allocate: klog_*! use
    // `alloc::format!` and would recursively fail. Use the spin writers (the
    // same no-alloc path the panic handler uses) so the used/total numbers
    // actually reach the console — they pinpoint whether this is a leak.
    //
    // BOTH consoles. A serial-only report is invisible on a box with just a
    // monitor, which is where this fires: one photographed OOM showed the
    // panic banner and the backtrace with NONE of the attribution below,
    // because all of it went to a serial line nobody was capturing.
    // `graphic_console_write_fmt_spin` is best-effort try_lock and allocates
    // nothing, so it cannot deadlock or recurse here.
    fn emit(args: core::fmt::Arguments<'_>) {
        kernel_hal::console::serial_write_fmt_spin(args);
        kernel_hal::console::graphic_console_write_fmt_spin(args);
    }

    let heap_used = crate::memory::heap_used();
    let heap_total = crate::memory::heap_total();
    emit(format_args!(
        "\nkernel OOM: alloc {} bytes failed (used {} / total {} MiB)\n",
        layout.size(),
        heap_used / 1024 / 1024,
        heap_total / 1024 / 1024,
    ));
    // A refusal from the heap re-entrancy guard reaches this handler as a null
    // pointer, i.e. as an allocation failure indistinguishable from a real
    // one. Say which it was: a guard event count that moved means the heap may
    // be fine and the fault is a fault path that allocates.
    let reentrancy = crate::memory::heap_reentrancy_events();
    if reentrancy > 0 {
        emit(format_args!(
            "heap re-entrancy guard has refused {} allocation(s) — if this OOM is one of \
             them the heap is not out of memory; see [heap-reentrant] above\n",
            reentrancy,
        ));
    }
    // Attribution: live allocations per size class, so the OOM report says
    // WHICH class holds the heap (each line: class upper bound, live count,
    // total bytes if every allocation were at the bound).
    #[cfg(all(target_arch = "x86_64", not(feature = "libos")))]
    {
        let hist = crate::memory::heap_live_histogram();
        emit(format_args!("heap live by size class:\n"));
        for (i, count) in hist.iter().enumerate() {
            if *count > 0 {
                let size = 1usize << i;
                emit(format_args!(
                    "  <={:>10}B x {:<8} (~{} MiB)\n",
                    size,
                    count,
                    (count * size) >> 20,
                ));
            }
        }
        // memfd attribution: the 4 KiB blocks are ramfs file pages, and the
        // desktop's big page consumers are wl_shm pools backed by memfd.
        #[cfg(feature = "linux")]
        {
            let (created, live, bytes) = linux_object::fs::memfd_stats();
            emit(format_args!(
                "memfd: created={} live={} live_bytes={} MiB\n",
                created,
                live,
                bytes >> 20,
            ));
        }
        emit(format_args!("hot exact sizes:\n"));
        for (size, live) in crate::memory::heap_hot_sizes() {
            if size != 0 && live > 0 {
                emit(format_args!(
                    "  {}B x {} (~{} MiB)\n",
                    size,
                    live,
                    (size * live) >> 20,
                ));
            }
        }
    }
    panic!("memory allocation of {} bytes failed", layout.size());
}

/// How many bytes a banner may occupy. A stack buffer, because the panic
/// handler must not allocate (the panic may BE an OOM) and must not depend on
/// any lock.
///
/// It was 1024 for a banner that was a header and a line per stuck site. Since
/// then the deadlock report grew a `non-acker` line per CPU that owes a TLB
/// shootdown ack, a `HOLDER cpuN is now at` line per holder, and the `DIAG:`
/// verdict -- and the buffer did not. A full slot table (~560 B), four
/// non-acker lines (~140 B each) and the verdict (~190 B) is a little over
/// 1300, so the report has been overflowing a 1024-byte buffer and losing
/// whatever came last, which was the conclusion. The comment on the HOLDER
/// lines shows the shape of it: they are capped at two because "anything added
/// here is spent out of the verdict's budget" -- rationing the buffer instead
/// of sizing it.
///
/// 2 KiB of a 2 MiB coroutine stack, once, on a machine that is already wedged.
/// [`tests::the_non_acker_cap_is_small_enough_to_leave_the_rest_of_the_banner_room`]
/// is what keeps this number ahead of what the banner prints.
const BANNER_BYTES: usize = 2048;

/// Bytes [`StackBuf::with_reserve`] holds back for the deadlock banner's final
/// verdict plus the `[+N B cut]` marker: the longest `DIAG:` line is ~190
/// bytes and the marker ~20.
const VERDICT_RESERVE: usize = 256;

/// Fixed-size, no-alloc formatter for the panic banner. The panic handler must
/// not allocate (the panic may BE an OOM) and must not depend on any lock.
///
/// `write_str` drops what does not fit, because a banner that panics while
/// reporting a panic reports nothing. Two things make that safe rather than
/// merely quiet:
///
/// * `reserve` holds bytes back at the end of the buffer, so the line written
///   after [`release_reserve`] lands whatever came before it. The deadlock
///   banner needs this: its `DIAG:` verdict is the one line that makes an
///   on-screen capture self-diagnosing, it is written last, and the block
///   before it -- one `non-acker cpuN ...` line per CPU that owes a TLB
///   shootdown ack -- is bounded only by the core count. Eight non-ackers at
///   ~120 bytes each is the whole buffer, and the conclusion was what fell off
///   the end. There is no scrolling back on a photograph of a wedged machine.
/// * `dropped` counts what was lost, so [`truncated`] can say so out loud
///   instead of leaving a reader to wonder whether the banner ended or was cut.
pub(crate) struct StackBuf {
    buf: [u8; BANNER_BYTES],
    len: usize,
    /// Bytes at the end of `buf` that `write_str` will not fill.
    reserve: usize,
    /// Bytes `write_str` dropped for want of room.
    dropped: usize,
}

impl StackBuf {
    /// A buffer whose whole length is writable.
    pub(crate) fn new() -> Self {
        Self {
            buf: [0u8; BANNER_BYTES],
            len: 0,
            reserve: 0,
            dropped: 0,
        }
    }

    /// A buffer that keeps `reserve` bytes back until [`release_reserve`].
    fn with_reserve(reserve: usize) -> Self {
        Self {
            reserve: reserve.min(BANNER_BYTES),
            ..Self::new()
        }
    }

    /// Open the reserved tail for writing. Call once, immediately before the
    /// line the reserve exists to protect.
    fn release_reserve(&mut self) {
        self.reserve = 0;
    }

    /// Bytes dropped for want of room, `0` when the banner is complete.
    fn dropped(&self) -> usize {
        self.dropped
    }

    /// The banner as a string. Truncation can split a multi-byte character, so
    /// what is returned is the valid prefix -- this used to be spelled out at
    /// each call site.
    pub(crate) fn valid_str(&self) -> &str {
        match core::str::from_utf8(&self.buf[..self.len]) {
            Ok(s) => s,
            Err(e) => core::str::from_utf8(&self.buf[..e.valid_up_to()]).unwrap_or(""),
        }
    }
}

impl core::fmt::Write for StackBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let cap = self.buf.len() - self.reserve;
        let room = cap.saturating_sub(self.len);
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        self.dropped += s.len() - n;
        Ok(())
    }
}

// ── Spinlock deadlock self-report ────────────────────────────────────────────
//
// Slots recording every CPU stuck >~8s on a spinlock (see kernel-sync's
// DEADLOCK_SPINS). The hook rebuilds a multi-line banner from ALL slots on
// each report, so a photo shows every stuck call site at once — both sides of
// an AB-BA deadlock, not just the last reporter. Lock-free by construction:
// atomics + the raw-framebuffer banner.
const DL_SLOTS: usize = 8;
static DL_FILE_PTR: [core::sync::atomic::AtomicUsize; DL_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; DL_SLOTS];
static DL_FILE_LEN: [core::sync::atomic::AtomicUsize; DL_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; DL_SLOTS];
static DL_LINE_CPU: [core::sync::atomic::AtomicUsize; DL_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; DL_SLOTS];
/// 1 = this slot is the lock HOLDER's acquire site (reported by a waiter's
/// snapshot), 0 = a stuck waiter's own call site. The distinction is the whole
/// point: the waiters are usually innocent readers; the holder line is the one
/// that names the wedged code path.
static DL_HOLDER: [core::sync::atomic::AtomicUsize; DL_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; DL_SLOTS];

/// `(line, cpu)` in one word: the line in the low half, the cpu in the high
/// half. One function so the pack and the two unpacks below cannot drift --
/// reading the pair back with a plain `as u32` is what dropped a whole CPU from
/// the banner (see [`dl_record`]).
const fn dl_pack(line: u32, cpu: u32) -> usize {
    ((cpu as usize) << 32) | line as usize
}

/// The line half of a packed word.
const fn dl_line(packed: usize) -> u32 {
    (packed & 0xffff_ffff) as u32
}

/// The cpu half of a packed word.
const fn dl_cpu(packed: usize) -> usize {
    packed >> 32
}

/// Record one `(site, cpu, role)` into the slots (deduplicated) — lock-free.
///
/// All three parts of that tuple are the key. The comparison used to mask the
/// packed word down to its line (`as u32`), so `cpu` was not in it: a second
/// CPU stuck at the SAME source line was dropped as a duplicate. That is not a
/// corner — it is the case this whole banner exists for. A shootdown convoy is
/// several CPUs wedged in one place, and the capture that prompted the HOLDER
/// lines had holder and waiter at the very same line (the kernel heap's
/// `dealloc`). The photograph then showed one CPU where several were stuck, and
/// "every stuck call site at once" quietly meant "one of them".
fn dl_record(ptr: usize, len: usize, line: u32, cpu: u32, holder: bool) {
    use core::sync::atomic::Ordering;
    let key = dl_pack(line, cpu);
    for i in 0..DL_SLOTS {
        let cur = DL_FILE_PTR[i].load(Ordering::SeqCst);
        if cur == ptr
            && DL_LINE_CPU[i].load(Ordering::SeqCst) == key
            && (DL_HOLDER[i].load(Ordering::SeqCst) != 0) == holder
        {
            return;
        }
        if cur == 0
            && DL_FILE_PTR[i]
                .compare_exchange(0, ptr, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            DL_FILE_LEN[i].store(len, Ordering::SeqCst);
            DL_LINE_CPU[i].store(key, Ordering::SeqCst);
            DL_HOLDER[i].store(holder as usize, Ordering::SeqCst);
            return;
        }
    }
}

/// The most `non-acker` lines the banner prints, and how many CPUs that leaves
/// out.
///
/// The cap exists for the same reason the HOLDER lines are capped at two: this
/// is the one block whose length is set by the core count rather than by the
/// slot table, at ~120 bytes a line, and it is written before the verdict.
/// Sixty-four of them is sixty times the buffer. The lines are repetitive -- a
/// convoy's non-ackers are usually wedged in the same place -- so the first few
/// carry the finding.
const MAX_NONACKER_LINES: usize = 4;

/// `(lines to print, CPUs left out)` for `total` non-acking CPUs.
const fn nonacker_lines(total: usize) -> (usize, usize) {
    if total <= MAX_NONACKER_LINES {
        (total, 0)
    } else {
        (MAX_NONACKER_LINES, total - MAX_NONACKER_LINES)
    }
}

/// How many slots hold a recorded site. Monotone (slots are claimed and never
/// released) and bounded by [`DL_SLOTS`], which is what makes it a safe key for
/// the serial re-emit guard.
fn dl_recorded_sites() -> usize {
    use core::sync::atomic::Ordering;
    (0..DL_SLOTS)
        .filter(|&i| DL_FILE_PTR[i].load(Ordering::SeqCst) != 0)
        .count()
}

/// Rebuild and paint the banner from all recorded slots.
fn dl_paint() {
    use core::fmt::Write;
    use core::sync::atomic::Ordering;
    // Reserved: the `DIAG:` verdict below is written last and is the line that
    // makes a photograph of a wedged machine self-diagnosing. Everything
    // between here and `release_reserve` may be cut; that line may not.
    let mut b = StackBuf::with_reserve(VERDICT_RESERVE);
    // The build id pins WHICH binary paniced: a stale build booted after a
    // fix landed reads exactly like the fix not working.
    let _ = write!(
        b,
        "DEADLOCK: spinlock(s) stuck >8s [build {}]",
        env!("ECLIPSE_BUILD_ID")
    );
    // Track whether any HOLDER is itself blocked in a TLB-shootdown ack-wait:
    // that is the "shootdown starvation" signature (convoy behind one CPU that
    // is waiting on a peer that never acks — a non-pumping IRQs-off spinner),
    // as opposed to a true lock-ordering AB-BA cycle.
    let mut shootdown_head = false;
    let mut nonack_union: u64 = 0;
    // The last CPU seen publishing a wait mask: whose goals the non-acker
    // lines below are held to (single-waiter in every capture so far).
    let mut waiter_cpu: usize = usize::MAX;
    // Every HOLDER cpu seen, so the RIP block below can say where each one is
    // wedged RIGHT NOW. The site printed on a HOLDER line is where it ACQUIRED
    // the lock, which for a lock with few acquire sites says almost nothing:
    // the capture that prompted this had HOLDER and waiter at the very same
    // line (the kernel heap's `dealloc`), so the banner named the lock and left
    // the only real question -- why is the holder not releasing it? -- unasked.
    let mut holder_cpus = [usize::MAX; DL_SLOTS];
    let mut holder_n = 0usize;
    for i in 0..DL_SLOTS {
        let p = DL_FILE_PTR[i].load(Ordering::SeqCst);
        if p == 0 {
            continue;
        }
        let l = DL_FILE_LEN[i].load(Ordering::SeqCst);
        let lc = DL_LINE_CPU[i].load(Ordering::SeqCst);
        let cpu = dl_cpu(lc);
        let is_holder = DL_HOLDER[i].load(Ordering::SeqCst) != 0;
        let role = if is_holder { "HOLDER " } else { "" };
        if is_holder && holder_n < DL_SLOTS {
            holder_cpus[holder_n] = cpu;
            holder_n += 1;
        }
        // SAFETY: (p, l) were stored from a live &'static str (either the
        // reporter's own #[track_caller] file, or the holder's, snapshotted by
        // kernel-sync from the same immortal strings).
        let f = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(p as *const u8, l))
        };
        let _ = write!(b, "\n{}cpu={} at {}:{}", role, cpu, f, dl_line(lc));
        // If this CPU is spin-waiting for a TLB-shootdown ack, name the CPUs it
        // is blocked on. A HOLDER shown here is the convoy head — the machine is
        // wedged not by a lock cycle but because those CPUs never acked.
        let mask = kernel_hal::shootdown_wait_mask(cpu);
        if mask != 0 {
            if is_holder {
                shootdown_head = true;
            }
            waiter_cpu = cpu;
            nonack_union |= mask;
            let _ = write!(b, " [TLB-ack wait, blocked on cpu");
            let mut m = mask;
            let mut first = true;
            while m != 0 {
                let c = m.trailing_zeros();
                let _ = write!(b, "{}{}", if first { " " } else { "," }, c);
                first = false;
                m &= m - 1;
            }
            let _ = write!(b, "]");
        }
    }
    // Name where each non-acking CPU actually is. The last-tick RIP is frozen
    // ONE TICK BEFORE the IRQs-off spin begins — it names where the CPU *was*,
    // not where it is wedged (on the RTX it pointed at a procfs quicksort the
    // CPU had already left). An NMI is delivered even to a core spinning with
    // IRQs disabled, so broadcast one first and read each stuck core's CURRENT
    // RIP: that is the exact non-pumping busy-wait to symbolize with
    // `llvm-addr2line -e zcore`. Do the broadcast ONCE, before the loop.
    // The pre-snapshot has to happen BEFORE the broadcast (see below), so it
    // runs first and on its own; the broadcast itself is unconditional.
    let mut pre_seq = [0u64; 64];
    let mut pre_goal = [0u64; 64];
    let mut pre_q = [(0usize, 0usize, 0usize, false, false); 64];
    if nonack_union != 0 {
        // Snapshot the protocol state BEFORE the RIP-capture NMI broadcast:
        // that broadcast runs the unconditional shootdown rescue on every
        // stuck CPU (flush + publish), so any state read AFTER it shows the
        // post-rescue world and masks the starvation being diagnosed. The
        // pre/post seq pair is the discriminator: pre<goal && post>=goal
        // means the rescue works and the starvation was real; post<goal
        // means the publish itself is not landing (identity / wrong slot).
        let mut m = nonack_union;
        while m != 0 {
            let c = m.trailing_zeros() as usize;
            m &= m - 1;
            if c < 64 {
                pre_seq[c] = kernel_hal::shootdown_seq_of(c);
                pre_goal[c] = kernel_hal::shootdown_goal(waiter_cpu, c);
                pre_q[c] = kernel_hal::shootdown_queue_state(c);
            }
        }
    }
    // One broadcast, always. An NMI reaches a core even while it spins with
    // IRQs off, and `nmi_rip` is a plain atomic read of a per-cpu slot -- no
    // heap, no lock -- which is what makes this safe here, where the allocator
    // itself may be the wedged lock. Both of these are lock-free:
    // `send_nmi_all_others` writes the local APIC directly and the settle wait
    // is an `rdtsc` spin.
    kernel_hal::kstats::capture_cpu_rips();
    if nonack_union != 0 {
        let mut m = nonack_union;
        // Capped, and the count of what was left out printed, for the same
        // reason the HOLDER lines are capped at two: this is the one block in
        // the banner whose length is set by the core count rather than by the
        // slot table, at ~120 bytes a line, and it is written BEFORE the
        // verdict. Sixty-four of them is sixty times the buffer. The lines are
        // repetitive -- a convoy's non-ackers are usually wedged in the same
        // place -- so the first few carry the finding and the rest only cost
        // the reader the conclusion.
        let (cap, omitted) = nonacker_lines(nonack_union.count_ones() as usize);
        let mut shown = 0usize;
        while m != 0 {
            let c = m.trailing_zeros() as usize;
            m &= m - 1;
            if shown == cap {
                let _ = write!(b, "\n… and {} more non-acking cpu(s) not shown", omitted);
                break;
            }
            shown += 1;
            let nmi = kernel_hal::kstats::nmi_rip(c);
            let tick = kernel_hal::kstats::cpu_tick_rip(c);
            let post_seq = kernel_hal::shootdown_seq_of(c);
            let (chead, ptail, phead, ack_active, overflow) = if c < 64 {
                pre_q[c]
            } else {
                (0, 0, 0, false, false)
            };
            let _ = write!(
                b,
                "\nnon-acker cpu{} nmi_rip={:#x} (last_tick={:#x}) seq={}->{} goal={} q={}/{}/{} fl={}{}",
                c,
                nmi,
                tick,
                if c < 64 { pre_seq[c] } else { 0 },
                post_seq,
                if c < 64 { pre_goal[c] } else { 0 },
                chead,
                ptail,
                phead,
                ack_active as u8,
                overflow as u8
            );
        }
    }
    // Where each HOLDER actually is, which is the question the banner exists to
    // answer and could not. A holder's printed site is where it ACQUIRED the
    // lock; this is the instruction it is sitting on now. Symbolized in place
    // when the kernel carries a symbol table (`ksyms::lookup` is a binary
    // search over a static blob -- no heap, no lock), so the common case needs
    // no addr2line round trip at all.
    // At most two. The banner is a 1024-byte stack buffer whose writer
    // silently truncates, and the DIAG verdict is written last -- so anything
    // added here is spent out of the verdict's budget. Two covers every
    // capture seen (one holder, or the two sides of a cycle) and leaves the
    // line that names the conclusion room to land.
    for &c in holder_cpus.iter().take(holder_n.min(2)) {
        let rip = kernel_hal::kstats::nmi_rip(c);
        if rip != 0 {
            let _ = write!(
                b,
                "\nHOLDER cpu{} is now at {}",
                c,
                kernel_hal::ksyms::Addr(rip)
            );
        } else {
            // No NMI landed: either that cpu is gone (halted, triple-faulted,
            // never started) or NMIs are blocked on it. Both are findings.
            let _ = write!(
                b,
                "\nHOLDER cpu{} took no NMI (last_tick={:#x}) -- cpu dead or NMI-blocked",
                c,
                kernel_hal::kstats::cpu_tick_rip(c)
            );
        }
    }
    // Everything above was allowed to be cut; what follows is not. The verdict
    // is the line a reader acts on, so it gets the bytes held back for it.
    b.release_reserve();
    // One-line verdict so the on-screen (no-serial) capture is self-diagnosing.
    if shootdown_head {
        let _ = write!(
            b,
            "\nDIAG: shootdown starvation — the HOLDER waits a TLB ack from a CPU \
             that never pumps; symbolize the non-acker nmi_rip above to name it. \
             Not AB-BA."
        );
    } else {
        // NOT "therefore AB-BA". Ruling out shootdown starvation leaves more
        // than one cause, and a cycle is only one of them: a holder looping or
        // merely slow inside its critical section (the buddy allocator's O(n)
        // coalescing scan is a real candidate), or one that faulted there,
        // produces this exact banner with no cycle anywhere. The capture that
        // prompted this wording had HOLDER and waiter on the SAME line, which
        // cannot be AB-BA at all -- that needs two locks taken in opposite
        // orders -- yet the verdict sent the reader hunting for one. The
        // "is now at" line above is what discriminates.
        let _ = write!(
            b,
            "\nDIAG: no HOLDER is in a shootdown wait. Either a lock-ordering cycle \
             (AB-BA) or a HOLDER stuck inside its own critical section -- the \
             \"is now at\" line above says which."
        );
    }
    // Say so when the banner did not all fit, rather than ending mid-line and
    // letting a reader take the cut for the end of the report.
    let cut = b.dropped();
    if cut > 0 {
        let _ = write!(b, "\n[{} B of this report did not fit]", cut);
    }
    let valid = b.valid_str();
    kernel_hal::console::panic_banner(valid);
    // …and to the serial console, which is the only one anybody watching a
    // headless run can see.
    //
    // `panic_banner` above rasterizes straight onto the framebuffer, and its
    // x86_64 implementation is `#[cfg(feature = "graphic")]` — so on a build
    // without graphics, driven over `-serial mon:stdio` with `-display none`
    // (which is exactly how the QEMU benchmark harness runs), the deadlock
    // detector was a complete no-op. A silent hang was therefore indistinguishable
    // from a hang with a fired-but-invisible deadlock report, and "no banner
    // appeared" got mistaken for "not a deadlock". It is not evidence unless it
    // can reach the observer.
    //
    // Spin rather than `try_lock`: by the time this fires the machine has been
    // wedged for eight seconds and a dropped report is a wasted debugging cycle.
    // The deadlock hook runs from inside a stuck lock acquisition, where
    // `push_off` has already disabled interrupts — the precondition
    // `serial_write_fmt_spin` documents.
    //
    // Re-emitted whenever the banner GAINS content, not once. The first report
    // is the waiter's, and its serial line goes out before the same waiter has
    // snapshotted the HOLDER — so a once-guard mailed out half the diagnosis
    // and permanently suppressed the line that names the culprit. On a
    // graphics-less run (the benchmark harness) that made the holder
    // unknowable: the framebuffer repaint had it, and nobody could see it.
    // Slot count bounds the reprints (each unique site is recorded once), so
    // this cannot storm: at most DL_SLOTS emissions ever.
    // Keyed on how many SITES have been recorded, which is what bounds the
    // reprints. It used to be keyed on the banner's byte length, and that is
    // not the same number: the block of `non-acker`/`is now at` lines is
    // rebuilt from live RIPs on every call, so its length moves without a new
    // site being recorded (extra emissions) and a genuinely new site can leave
    // the length unchanged (a suppressed one). Worse once the banner is long
    // enough to be cut: the length then stops growing at the buffer size, so
    // the HOLDER line -- the one a once-guard was changed to stop losing --
    // would never be mailed out at all.
    static DL_SERIAL_SITES: core::sync::atomic::AtomicUsize =
        core::sync::atomic::AtomicUsize::new(0);
    let sites = dl_recorded_sites();
    let prev = DL_SERIAL_SITES.load(Ordering::SeqCst);
    if sites > prev
        && DL_SERIAL_SITES
            .compare_exchange(prev, sites, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        kernel_hal::console::serial_write_fmt_spin(format_args!("\n[{}]\n", valid));
    }
}

pub fn deadlock_report(file: &'static str, line: u32) {
    let cpu = kernel_hal::cpu::cpu_id() as u32;
    dl_record(file.as_ptr() as usize, file.len(), line, cpu, false);
    dl_paint();
}

/// Twin of [`deadlock_report`] for the lock HOLDER, called by a stuck waiter
/// with the holder's acquire site snapshotted from the lock itself (see
/// kernel-sync's `set_deadlock_holder_hook`). `cpu` is the cpu that acquired
/// the lock, not the reporter's.
pub fn deadlock_holder_report(file_ptr: usize, file_len: usize, line: u32, cpu: u32) {
    dl_record(file_ptr, file_len, line, cpu, true);
    dl_paint();
}

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Disable interrupts immediately. With panic-strategy=abort, local variables
    // in the panicking function (e.g. kernel-sync's RefMut borrow guard in
    // pop_off) are never dropped. If a timer IRQ fires while the panic handler
    // is running, push_off/pop_off will call borrow_mut() on an already-borrowed
    // RefCell → nested panic → abort() → ud2 → triple fault → QEMU reset.
    kernel_hal::interrupt::intr_off();

    // Before any console output at all: tell the graphic console to stop
    // trusting its cell cache. Three boots in a row died as a panic INSIDE this
    // handler, on the console path, because the fault being reported had
    // already corrupted the buffer the console was about to resize and repaint.
    kernel_hal::console::note_panicking();

    // FIRST, before anything that touches a lock: rasterize the panic straight
    // onto the framebuffer (red band, raw pixel writes, no locks, no alloc).
    // Everything below can be silently dropped or deadlock when another CPU —
    // or THIS one — holds the console/serial locks (a panic inside an IRQ
    // handler mid-print left the screen frozen half-line with the real panic
    // visible only on serial). This banner cannot.
    {
        use core::fmt::Write;
        let mut b = StackBuf::new();
        if let Some(loc) = info.location() {
            let _ = write!(
                b,
                "KERNEL PANIC cpu={} {}:{}\n{}",
                kernel_hal::cpu::cpu_id(),
                loc.file(),
                loc.line(),
                info.message()
            );
        } else {
            let _ = write!(
                b,
                "KERNEL PANIC cpu={}\n{}",
                kernel_hal::cpu::cpu_id(),
                info.message()
            );
        }
        kernel_hal::console::panic_banner(b.valid_str());
    }

    // Make the panic VISIBLE after a compositor took the screen. Once labwc
    // sets KD_GRAPHICS the kernel stops PRESENTING the text console (writes
    // still land in the shadow buffer but are never pushed to the display), so
    // a panic here would only reach serial — the monitor stays black on the
    // compositor's last frame and the crash reads as a silent freeze. Forcing
    // the active VT back to KD_TEXT repaints the text console and makes every
    // graphic_console_write_fmt below actually appear on the monitor. It is
    // panic-safe: the repaint is best-effort try_lock and allocates nothing.
    //
    // Remembered, not just set: if `oops` manages to contain this fault the
    // machine keeps running, and leaving the VT in KD_TEXT would strand a live
    // compositor rendering into a buffer that is no longer presented — the
    // desktop would look frozen even though nothing but one process died.
    let prev_kd = kernel_hal::console::kd_mode();
    kernel_hal::console::set_kd_mode(kernel_hal::console::KD_TEXT);

    // Use spin variant: interrupts are already off above, and try_lock silently
    // discards output if another CPU holds the lock — unacceptable in panic context.
    //
    // Mirror to the graphic console too: on a real bring-up box with only a
    // monitor (no serial capture), a serial-only panic is invisible and reads
    // as a silent freeze. graphic_console_write_fmt is a best-effort try_lock
    // that no-ops if the VT lock is held, so it can't deadlock the panic path.
    // A panic that names a cpu the machine does not have is not a typo in the
    // report — it means GS was lying, and everything this CPU did with
    // `push_off`/`pop_off` since then landed on a foreign per-CPU slot. Say so
    // next to the panic instead of leaving "cpu=48 on a 6-vCPU guest" to be
    // squinted at.
    {
        let (last, count) = lock::bogus_cpu_id_events();
        if count > 0 {
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "\n[cpuid-bogus] GS reported logical cpu {} ({} time(s)) naming no \
                 registered CPU — push_off/pop_off nested on a foreign per-CPU slot\n",
                last, count,
            ));
        }
    }
    if let Some(loc) = info.location() {
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "\n\npanic cpu={} at {}:{}:{}\n",
            kernel_hal::cpu::cpu_id(),
            loc.file(),
            loc.line(),
            loc.column(),
        ));
        kernel_hal::console::graphic_console_write_fmt_spin(format_args!(
            "\n\n[PANIC] cpu={} at {}:{}:{}\n",
            kernel_hal::cpu::cpu_id(),
            loc.file(),
            loc.line(),
            loc.column(),
        ));
    } else {
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "\n\npanic cpu={}\n",
            kernel_hal::cpu::cpu_id(),
        ));
        kernel_hal::console::graphic_console_write_fmt_spin(format_args!(
            "\n\n[PANIC] cpu={}\n",
            kernel_hal::cpu::cpu_id(),
        ));
    }
    // `as_str()` returns None for any panic! with format arguments — use
    // Display on the Arguments directly so the message is always printed.
    kernel_hal::console::serial_write_fmt_spin(format_args!("{}\n", info.message()));
    kernel_hal::console::graphic_console_write_fmt_spin(format_args!("{}\n", info.message()));

    // Frame-pointer backtrace: walk the saved RBP chain and print return
    // addresses so a panic in an inlined helper (e.g. x86_64::VirtAddr::new
    // called from deep in the paging code) can be mapped back to the real
    // caller with `nm`/`addr2line`. Best-effort and bounded: stop on a null /
    // unaligned / non-canonical frame pointer so a corrupt chain can't fault
    // the panic handler itself.
    #[cfg(target_arch = "x86_64")]
    {
        let mut rbp: usize;
        unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp) };
        // Mirror the backtrace to BOTH serial and the graphic console. On a
        // headless-but-monitor'd bring-up box (the real-HW case) the operator
        // only ever sees the graphic framebuffer -- serial-only backtraces are
        // invisible in a phone photo of the screen, which is the only artifact
        // that comes back. Printing the return addresses on the framebuffer too
        // means a photo of the panic is enough to `addr2line` the culprit.
        kernel_hal::console::serial_write_fmt_spin(format_args!("[backtrace]\n"));
        kernel_hal::console::graphic_console_write_fmt_spin(format_args!("[backtrace]\n"));
        for _ in 0..32 {
            if rbp == 0 || rbp & 0x7 != 0 || rbp < 0xffff_8000_0000_0000 {
                break;
            }
            let next = unsafe { core::ptr::read_volatile(rbp as *const usize) };
            let ret = unsafe { core::ptr::read_volatile((rbp + 8) as *const usize) };
            if ret == 0 {
                break;
            }
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "  ret={}\n",
                kernel_hal::ksyms::Addr(ret as u64)
            ));
            kernel_hal::console::graphic_console_write_fmt_spin(format_args!(
                "  ret={}\n",
                kernel_hal::ksyms::Addr(ret as u64)
            ));
            if next <= rbp {
                break; // stack grows down; a non-increasing frame is corrupt
            }
            rbp = next;
        }
    }

    // How many kernel faults this boot has already survived. A panic arriving
    // behind others that were contained is usually the same root cause coming
    // back, and that is worth knowing from the banner alone.
    let contained = crate::oops::contained_count();
    if contained > 0 {
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "[oops] kernel faults already contained this boot: {}\n",
            contained
        ));
    }

    // Whether getting this report out required taking the serial lock away from
    // somebody. Non-zero means a CPU was holding it and never gave it back --
    // it died mid-print, or is wedged -- so the lines above may be interleaved
    // with half of its line, and there is a second casualty besides this panic.
    // Snapshotted before the write, which would otherwise count itself.
    let steals = kernel_hal::console::serial_lock_steals();
    if steals > 0 {
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "[console] serial lock taken from a holder that never returned it: {} time(s) \
             -- output above may be interleaved\n",
            steals
        ));
    }

    // Last resort before halting: if this panic happened while serving one
    // particular task -- and only then -- kill that task and hand the CPU back
    // to the scheduler instead of taking the whole system down. Does not return
    // if it succeeds; if it returns it has already said why it could not, and
    // we halt as always. Not attempted under `baremetal-test`, where a panic
    // *must* end the machine so the test fails.
    if !cfg!(feature = "baremetal-test") {
        crate::oops::try_contain("kernel panic", Some(prev_kd));
    }

    if cfg!(feature = "baremetal-test") {
        kernel_hal::cpu::reset();
    } else {
        loop {
            core::hint::spin_loop();
        }
    }
}

/// The banner builder and the deadlock slots, on the host.
///
/// Neither had ever been compiled by a test binary: this module is
/// `#[cfg(not(feature = "libos"))]` in the kernel build and `cargo test -p
/// zcore` runs with `--features libos`, so the panic path and the deadlock
/// report were written, twice rewritten, and never once executed by a job.
/// Only `#[panic_handler]` and `#[alloc_error_handler]` genuinely cannot be
/// here (std supplies both); everything below is the code the machine runs.
#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write;
    use core::sync::atomic::Ordering;

    /// One test at a time through the deadlock slots, which are process-wide
    /// statics that every test would otherwise believe are its own -- and which
    /// are deliberately never cleared in the kernel, so they cannot be reset by
    /// the code under test. Saved and restored, so a test leaves the slots as it
    /// found them even on a panic.
    fn alone_with_the_slots(body: impl FnOnce()) {
        extern crate std;
        use std::sync::Mutex;
        static TURNSTILE: Mutex<()> = Mutex::new(());
        let _guard = TURNSTILE.lock().unwrap_or_else(|e| e.into_inner());
        let saved: [(usize, usize, usize, usize); DL_SLOTS] = core::array::from_fn(|i| {
            (
                DL_FILE_PTR[i].load(Ordering::SeqCst),
                DL_FILE_LEN[i].load(Ordering::SeqCst),
                DL_LINE_CPU[i].load(Ordering::SeqCst),
                DL_HOLDER[i].load(Ordering::SeqCst),
            )
        });
        for i in 0..DL_SLOTS {
            DL_FILE_PTR[i].store(0, Ordering::SeqCst);
            DL_FILE_LEN[i].store(0, Ordering::SeqCst);
            DL_LINE_CPU[i].store(0, Ordering::SeqCst);
            DL_HOLDER[i].store(0, Ordering::SeqCst);
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        for (i, (p, l, lc, h)) in saved.iter().enumerate() {
            DL_FILE_PTR[i].store(*p, Ordering::SeqCst);
            DL_FILE_LEN[i].store(*l, Ordering::SeqCst);
            DL_LINE_CPU[i].store(*lc, Ordering::SeqCst);
            DL_HOLDER[i].store(*h, Ordering::SeqCst);
        }
        if let Err(e) = r {
            std::panic::resume_unwind(e);
        }
    }

    /// The `(file, line, cpu, role)` of one slot, as the banner would print it.
    fn slot(i: usize) -> Option<(&'static str, u32, usize, bool)> {
        let p = DL_FILE_PTR[i].load(Ordering::SeqCst);
        if p == 0 {
            return None;
        }
        let l = DL_FILE_LEN[i].load(Ordering::SeqCst);
        let lc = DL_LINE_CPU[i].load(Ordering::SeqCst);
        // SAFETY: the test stored these from a `&'static str` literal.
        let f = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(p as *const u8, l))
        };
        Some((
            f,
            dl_line(lc),
            dl_cpu(lc),
            DL_HOLDER[i].load(Ordering::SeqCst) != 0,
        ))
    }

    fn record(file: &'static str, line: u32, cpu: u32, holder: bool) {
        dl_record(file.as_ptr() as usize, file.len(), line, cpu, holder);
    }

    // ── The banner buffer ───────────────────────────────────────────────────

    #[test]
    fn a_banner_that_fits_comes_back_whole_and_drops_nothing() {
        let mut b = StackBuf::new();
        let _ = write!(b, "DEADLOCK: cpu={} at {}:{}", 3, "src/lib.rs", 42);
        assert_eq!(b.valid_str(), "DEADLOCK: cpu=3 at src/lib.rs:42");
        assert_eq!(b.dropped(), 0);
    }

    #[test]
    fn a_banner_past_the_buffer_keeps_its_head_and_counts_what_it_lost() {
        let mut b = StackBuf::new();
        // Written in chunks, as `format_args!` feeds the writer, and past the
        // end of the buffer whatever its size is.
        let chunk = "x".repeat(100);
        let chunks = BANNER_BYTES / 100 + 3;
        for _ in 0..chunks {
            let _ = write!(b, "{}", chunk);
        }
        assert_eq!(b.valid_str().len(), BANNER_BYTES);
        assert_eq!(b.dropped(), chunks * 100 - BANNER_BYTES);
        assert!(b.valid_str().starts_with("xxx"));
    }

    /// The reserve is not spendable by the lines that come before it: that is
    /// the whole mechanism, and the number it protects is the buffer size minus
    /// the reserve, not the buffer size.
    #[test]
    fn the_lines_before_the_verdict_cannot_spend_the_bytes_held_back_for_it() {
        let mut b = StackBuf::with_reserve(VERDICT_RESERVE);
        let _ = write!(b, "{}", "y".repeat(BANNER_BYTES * 2));
        assert_eq!(b.valid_str().len(), BANNER_BYTES - VERDICT_RESERVE);
        assert!(b.dropped() > 0);
    }

    /// The bug, in the shape the machine produces it: a header, then a block of
    /// repetitive `non-acker` lines long enough to fill the banner, and then the
    /// one line a reader acts on. Without the reserve the verdict is what falls
    /// off the end -- on a photograph of a wedged machine, with nothing to
    /// scroll back to.
    #[test]
    fn the_verdict_lands_even_when_the_block_before_it_filled_the_banner() {
        const VERDICT: &str = "\nDIAG: no HOLDER is in a shootdown wait. Either a \
                               lock-ordering cycle (AB-BA) or a HOLDER stuck inside \
                               its own critical section -- the \"is now at\" line \
                               above says which.";
        let mut b = StackBuf::with_reserve(VERDICT_RESERVE);
        let _ = write!(b, "DEADLOCK: spinlock(s) stuck >8s [build deadbeef]");
        for c in 0..64 {
            let _ = write!(
                b,
                "\nnon-acker cpu{} nmi_rip={:#x} (last_tick={:#x}) seq={}->{} \
                 goal={} q={}/{}/{} fl={}{}",
                c, 0xffff_ff00_0007_3bf2u64, 0xffff_ff00_0007_3bf2u64, 7, 7, 9, 1, 2, 3, 1, 0
            );
        }
        assert!(
            b.dropped() > 0,
            "the block should have overflowed the banner"
        );
        b.release_reserve();
        let _ = write!(b, "{}", VERDICT);
        assert!(
            b.valid_str().ends_with("above says which."),
            "the verdict was cut: banner ends {:?}",
            &b.valid_str()[b.valid_str().len() - 40..]
        );
        assert!(b.valid_str().starts_with("DEADLOCK: spinlock(s) stuck >8s"));
    }

    #[test]
    fn a_banner_says_so_when_it_did_not_all_fit() {
        let mut b = StackBuf::with_reserve(VERDICT_RESERVE);
        let _ = write!(b, "{}", "z".repeat(BANNER_BYTES * 2));
        b.release_reserve();
        let cut = b.dropped();
        let _ = write!(b, "\n[{} B of this report did not fit]", cut);
        assert!(b.valid_str().ends_with("B of this report did not fit]"));
        assert!(cut > 0);
    }

    /// A cut can land in the middle of a multi-byte character, and the banner is
    /// handed on as a `&str`.
    #[test]
    fn a_character_the_cut_split_in_half_is_not_in_the_banner() {
        let mut b = StackBuf::new();
        let _ = write!(b, "{}", "a".repeat(BANNER_BYTES - 1));
        // 'é' is two bytes and only one byte of room is left.
        let _ = write!(b, "é");
        assert_eq!(b.dropped(), 1, "one byte of the pair should have been kept");
        let s = b.valid_str();
        assert_eq!(s.len(), BANNER_BYTES - 1, "the half character is not in it");
        assert!(s.chars().all(|c| c == 'a'));
    }

    #[test]
    fn a_reserve_bigger_than_the_buffer_leaves_no_room_instead_of_panicking() {
        let mut b = StackBuf::with_reserve(BANNER_BYTES * 4);
        let _ = write!(b, "anything at all");
        assert_eq!(b.valid_str(), "");
        assert_eq!(b.dropped(), "anything at all".len());
        b.release_reserve();
        let _ = write!(b, "the verdict");
        assert_eq!(b.valid_str(), "the verdict");
    }

    // ── The packed (line, cpu) word ─────────────────────────────────────────

    #[test]
    fn the_line_and_the_cpu_come_back_out_of_the_packed_word() {
        for (line, cpu) in [(1u32, 0u32), (42, 3), (u32::MAX, 63), (0, 63)] {
            let p = dl_pack(line, cpu);
            assert_eq!(dl_line(p), line, "line of ({line}, {cpu})");
            assert_eq!(dl_cpu(p), cpu as usize, "cpu of ({line}, {cpu})");
        }
    }

    /// The comparison that dropped a CPU: masked to `as u32` these two are the
    /// same word.
    #[test]
    fn the_same_line_on_two_cpus_is_two_different_keys() {
        assert_ne!(dl_pack(1200, 0), dl_pack(1200, 1));
        assert_eq!(dl_line(dl_pack(1200, 0)), dl_line(dl_pack(1200, 1)));
    }

    // ── The slots ───────────────────────────────────────────────────────────

    /// The bug. A shootdown convoy is several CPUs wedged in one place, and the
    /// capture that prompted the HOLDER lines had holder and waiter at the very
    /// same line, so this is the ordinary case and not a corner. The banner
    /// showed one of them.
    #[test]
    fn two_cpus_stuck_at_the_same_line_are_both_recorded() {
        alone_with_the_slots(|| {
            record("linux-object/src/fs/pty.rs", 1200, 0, false);
            record("linux-object/src/fs/pty.rs", 1200, 3, false);
            assert_eq!(dl_recorded_sites(), 2, "the second cpu was dropped");
            assert_eq!(slot(0).map(|s| s.2), Some(0));
            assert_eq!(slot(1).map(|s| s.2), Some(3));
        });
    }

    #[test]
    fn the_same_cpu_reporting_the_same_line_again_is_recorded_once() {
        alone_with_the_slots(|| {
            for _ in 0..10 {
                record("kernel-hal/src/mem.rs", 88, 2, false);
            }
            assert_eq!(dl_recorded_sites(), 1);
        });
    }

    /// The role is part of the key too: one CPU can be a waiter at a line while
    /// another CPU holds the lock acquired at that same line, and the HOLDER
    /// line is the one that names the wedged path.
    #[test]
    fn a_waiter_and_a_holder_at_one_line_are_two_records() {
        alone_with_the_slots(|| {
            record("zCore/src/memory.rs", 77, 1, false);
            record("zCore/src/memory.rs", 77, 1, true);
            assert_eq!(dl_recorded_sites(), 2);
            assert_eq!(slot(0).map(|s| s.3), Some(false));
            assert_eq!(slot(1).map(|s| s.3), Some(true));
        });
    }

    #[test]
    fn the_slots_fill_up_and_then_keep_what_they_have() {
        alone_with_the_slots(|| {
            for cpu in 0..(DL_SLOTS as u32 + 4) {
                record("a/b.rs", 5, cpu, false);
            }
            assert_eq!(dl_recorded_sites(), DL_SLOTS);
            // The first reporters are the ones kept, not the last.
            assert_eq!(slot(0).map(|s| s.2), Some(0));
            assert_eq!(slot(DL_SLOTS - 1).map(|s| s.2), Some(DL_SLOTS - 1));
        });
    }

    /// The cap on the one block whose length the core count sets. Under the cap
    /// nothing is hidden and nothing is claimed to be; over it the reader is
    /// told exactly how many CPUs are missing, because "and 2 more" and "and 59
    /// more" are different findings.
    #[test]
    fn the_non_acker_block_says_how_many_cpus_it_left_out() {
        assert_eq!(nonacker_lines(0), (0, 0));
        assert_eq!(nonacker_lines(1), (1, 0));
        assert_eq!(nonacker_lines(MAX_NONACKER_LINES), (MAX_NONACKER_LINES, 0));
        assert_eq!(
            nonacker_lines(MAX_NONACKER_LINES + 1),
            (MAX_NONACKER_LINES, 1)
        );
        assert_eq!(
            nonacker_lines(64),
            (MAX_NONACKER_LINES, 64 - MAX_NONACKER_LINES)
        );
        // Whatever the count, every CPU is either printed or counted.
        for total in 0..=64 {
            let (shown, omitted) = nonacker_lines(total);
            assert_eq!(shown + omitted, total, "{total} non-ackers");
            assert!(shown <= MAX_NONACKER_LINES);
        }
    }

    /// How big the cap may be, said WITHOUT naming the cap -- otherwise the
    /// test above moves with it and a cap of 64 passes, which is how a first
    /// pass at this let the mutant live.
    ///
    /// Two independent bounds. The block is supplementary to the slot table, so
    /// printing more non-ackers than there are slots for actual stuck sites
    /// inverts the banner's priorities; and one line, measured here rather than
    /// guessed, times the cap must still leave the slot table and the verdict
    /// their room in the buffer.
    #[test]
    fn the_non_acker_cap_is_small_enough_to_leave_the_rest_of_the_banner_room() {
        assert!(
            MAX_NONACKER_LINES * 2 <= DL_SLOTS,
            "more non-acker lines ({}) than half the slot table ({})",
            MAX_NONACKER_LINES,
            DL_SLOTS
        );
        // One line at the magnitudes the kernel prints: a kernel-half RIP, a
        // shootdown sequence and goal, three queue indices and two flags.
        let mut one = StackBuf::new();
        let _ = write!(
            one,
            "\nnon-acker cpu{} nmi_rip={:#x} (last_tick={:#x}) seq={}->{} \
             goal={} q={}/{}/{} fl={}{}",
            63usize,
            0xffff_ff00_0007_3bf2u64,
            0xffff_ff00_0007_3bf2u64,
            u32::MAX,
            u32::MAX,
            u32::MAX,
            4095,
            4095,
            4095,
            1,
            1
        );
        let line = one.valid_str().len();
        assert_eq!(one.dropped(), 0, "the measurement itself was cut");
        // The slot table is what the banner is chiefly for: DL_SLOTS lines of a
        // path and a number, ~70 bytes each.
        let slot_table = DL_SLOTS * 70;
        assert!(
            MAX_NONACKER_LINES * line + slot_table + VERDICT_RESERVE <= BANNER_BYTES,
            "{} non-acker lines of {line} B plus the slot table ({slot_table} B) \
             and the verdict ({VERDICT_RESERVE} B) do not fit in {BANNER_BYTES} B",
            MAX_NONACKER_LINES
        );
    }

    /// What the serial re-emit guard is keyed on, now that it is not keyed on
    /// the banner's byte length: monotone, and bounded by the slot count.
    #[test]
    fn the_recorded_site_count_rises_once_per_new_site_and_stops_at_the_slots() {
        alone_with_the_slots(|| {
            assert_eq!(dl_recorded_sites(), 0);
            for cpu in 0..(DL_SLOTS as u32) {
                record("a/b.rs", 5, cpu, false);
                assert_eq!(dl_recorded_sites(), cpu as usize + 1);
                // Reporting again does not move it.
                record("a/b.rs", 5, cpu, false);
                assert_eq!(dl_recorded_sites(), cpu as usize + 1);
            }
            record("c/d.rs", 9, 40, false);
            assert_eq!(dl_recorded_sites(), DL_SLOTS);
        });
    }
}
