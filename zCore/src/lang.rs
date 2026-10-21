// Rust language features implementations

use core::alloc::Layout;
use core::panic::PanicInfo;

/// Rows of each OOM attribution table that reach the screen.
///
/// The kernel OOM report is written to a console that cannot scroll (the early
/// framebuffer console wraps and paints over its own first lines), so the
/// budget is one screen for the whole thing: report, panic banner and
/// backtrace, on a screen as small as 1024x768 (48 rows of the 16-pixel
/// font). Six rows of each table, ranked by bytes, is what the two
/// photographed OOMs would have needed to name their eater; the classes below
/// them are summarised in one line rather than dropped.
#[cfg(any(all(target_arch = "x86_64", not(feature = "libos")), test))]
const OOM_TOP_ROWS: usize = 6;

/// Call sites printed per attribution table. Lower than [`OOM_TOP_ROWS`]
/// because each of these lines carries symbolised return addresses and is
/// wide enough to wrap.
#[cfg(any(all(target_arch = "x86_64", not(feature = "libos")), test))]
const OOM_TOP_SITES: usize = 4;

/// Return addresses printed per call site (the tables keep more).
#[cfg(any(all(target_arch = "x86_64", not(feature = "libos")), test))]
const OOM_SITE_FRAMES: usize = 2;

/// The `N` heaviest rows of a `(unit_bytes, live_count)` table, heaviest
/// first, plus how many rows were left out and how many bytes they held.
///
/// Allocation-free by construction — it is called from the
/// `#[alloc_error_handler]`, where `alloc::vec` would re-enter the failure it
/// is reporting — so the ranking is an insertion sort over a fixed array
/// rather than a `sort_by_key` over a `Vec`.
///
/// Rows with a zero count or a zero unit are not rows: they are the empty
/// slots of the fixed tables these come from, and counting them as "left out"
/// would put `(+19 lighter classes, ~0 MiB)` under every report.
#[cfg(any(all(target_arch = "x86_64", not(feature = "libos")), test))]
fn top_rows_by_bytes<const N: usize>(
    rows: impl Iterator<Item = (usize, usize)>,
) -> ([(usize, usize); N], usize, usize) {
    let mut top = [(0usize, 0usize); N];
    let mut omitted = 0usize;
    let mut omitted_bytes = 0usize;
    let note_omitted = |unit: usize, count: usize, omitted: &mut usize, bytes: &mut usize| {
        if count == 0 {
            return;
        }
        *omitted += 1;
        *bytes = bytes.saturating_add(unit.saturating_mul(count));
    };
    for (unit, count) in rows {
        if unit == 0 || count == 0 {
            continue;
        }
        let bytes = unit.saturating_mul(count);
        let mut pos = N;
        for (i, (u, c)) in top.iter().enumerate() {
            if bytes > u.saturating_mul(*c) {
                pos = i;
                break;
            }
        }
        if pos == N {
            note_omitted(unit, count, &mut omitted, &mut omitted_bytes);
            continue;
        }
        let (du, dc) = top[N - 1];
        note_omitted(du, dc, &mut omitted, &mut omitted_bytes);
        let mut i = N - 1;
        while i > pos {
            top[i] = top[i - 1];
            i -= 1;
        }
        top[pos] = (unit, count);
    }
    (top, omitted, omitted_bytes)
}

/// Sentinel for "no emergency reporter has claimed the screen yet".
///
/// Not a KD mode: `normalize_kd_mode` accepts only 0..=3, so `u32::MAX` can
/// never be a mode somebody actually set.
const KD_UNCLAIMED: u32 = u32::MAX;

/// The KD mode the active VT was in before the first emergency reporter took
/// the screen, or [`KD_UNCLAIMED`].
static PRE_REPORT_KD: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(KD_UNCLAIMED);

/// The mode to put the VT back in once a contained fault lets the machine run
/// on: the one it was in before the FIRST reporter claimed the screen, not the
/// one it is in now.
///
/// By the time the panic handler reads the live mode, a reporter that ran
/// ahead of it has already forced `KD_TEXT`, so the live value would restore
/// text mode over a compositor that is still alive and still rendering -- the
/// desktop would look frozen although only one process died. Pure, because
/// this is the whole of the decision and none of it needs a VT.
fn kd_restore_target(pre_report: u32, live: u32) -> u32 {
    if pre_report == KD_UNCLAIMED {
        live
    } else {
        pre_report
    }
}

/// Put the screen where an emergency report can be read, ONCE.
///
/// Everything the OOM report and the panic handler print goes to the early
/// framebuffer console, which draws straight into the framebuffer. A VT
/// repaint paints over all of it. So the first reporter forces `KD_TEXT` --
/// which repaints -- and every reporter after it finds the screen already
/// claimed and does not repaint again.
///
/// This is not a refinement: on 2026-10-21 a desktop boot with labwc holding
/// tty7 was photographed with the init log as a background and the panic
/// banner on top, and NOTHING of the heap attribution in between. The report
/// had been drawn; the panic handler's own `set_kd_mode(KD_TEXT)` then fell
/// back to tty1 and repainted it over the top.
fn claim_text_console() {
    use core::sync::atomic::Ordering;
    let live = kernel_hal::console::kd_mode();
    if PRE_REPORT_KD
        .compare_exchange(KD_UNCLAIMED, live, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // Somebody is already reporting on this screen. Repainting now would
        // paint over what they wrote.
        return;
    }
    kernel_hal::console::set_kd_mode(kernel_hal::console::KD_TEXT);
}

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

    // BEFORE the first line: take the screen off whatever compositor owns it.
    // Forcing KD_TEXT repaints the text console, and a repaint paints over
    // everything the early framebuffer console has drawn -- so it has to
    // happen first, not after the report is on the glass. `claim_text_console`
    // also stops the panic handler below from repainting a second time.
    claim_text_console();

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
    // Blocks the allocator refused because they were not its own. Each one is
    // leaked on purpose (see `heap_regions`), so a non-zero count both
    // explains bytes missing from this heap and says that something in the
    // kernel is freeing memory it does not own -- which is a far more serious
    // finding than the OOM it may have caused.
    let wild = crate::memory::heap_wild_blocks();
    if wild > 0 {
        emit(format_args!(
            "heap has refused and LEAKED {} wild block(s), {} KiB in all -- something freed \
             memory the heap never handed out; see [heap-wild] above for the call chain\n",
            wild,
            crate::memory::heap_wild_bytes() >> 10,
        ));
    }
    // Free blocks caught written after their free, or freed twice. The front
    // cache leaks each one together with the rest of its list, so a non-zero
    // count also explains up to 4 MiB per report missing from this heap.
    let uaf = crate::memory::heap_written_after_free();
    if uaf > 0 {
        emit(format_args!(
            "heap has caught {} free block(s) written after their free, or freed twice, \
             and leaked them with their lists; see [heap-uaf] above\n",
            uaf,
        ));
    }
    // Attribution: live allocations per size class, so the OOM report says
    // WHICH class holds the heap (each line: class upper bound, live count,
    // total bytes if every allocation were at the bound).
    #[cfg(all(target_arch = "x86_64", not(feature = "libos")))]
    {
        // Every line below lands on a console that CANNOT scroll: the early
        // framebuffer console wraps, so a report longer than one screen
        // overwrites its own head -- and the head is where the heaviest
        // classes are. Both tables are therefore capped at their heaviest
        // `OOM_TOP_ROWS` rows, with one line for what was left out, so the
        // whole report plus the panic banner and backtrace fit on one
        // photograph. Two of them have already been taken with NOTHING of
        // this on them.
        let hist = crate::memory::heap_live_histogram();
        let (classes, more_classes, more_class_bytes) = top_rows_by_bytes::<OOM_TOP_ROWS>(
            hist.iter().enumerate().map(|(i, c)| (1usize << i, *c)),
        );
        emit(format_args!("heap live by size class (heaviest first):\n"));
        for (size, count) in classes {
            if count == 0 {
                continue;
            }
            emit(format_args!(
                "  <={:>10}B x {:<8} (~{} MiB)\n",
                size,
                count,
                (count * size) >> 20,
            ));
        }
        if more_classes > 0 {
            emit(format_args!(
                "  (+{} lighter class(es), ~{} MiB in all)\n",
                more_classes,
                more_class_bytes >> 20,
            ));
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
        emit(format_args!("hot exact sizes (heaviest first):\n"));
        let (hot, more_hot, more_hot_bytes) =
            top_rows_by_bytes::<OOM_TOP_ROWS>(crate::memory::heap_hot_sizes().into_iter());
        for (size, live) in hot {
            if live == 0 {
                continue;
            }
            emit(format_args!(
                "  {}B x {} (~{} MiB)\n",
                size,
                live,
                (size * live) >> 20,
            ));
        }
        if more_hot > 0 {
            emit(format_args!(
                "  (+{} lighter size(s), ~{} MiB in all)\n",
                more_hot,
                more_hot_bytes >> 20,
            ));
        }
        // WHO holds the blocks, not just how big they are. `/proc/kheap` and
        // the `[heap-pressure]` report have named the call sites since #1820,
        // but that reporter builds a `String` and so cannot run from here:
        // the one report that fires when the heap is ACTUALLY gone was the
        // only one without them. These two tables are alloc-free (fixed
        // atomics + `ksyms::Addr`, which formats straight out of the symbol
        // blob), so they can.
        let (big, big_missed) = crate::memory::heap_big_blocks();
        if big.iter().any(|(sz, _)| *sz > 0) {
            emit(format_args!(
                "live blocks >= 2 MiB, by allocating call site:\n"
            ));
            let mut shown = 0usize;
            for (sz, site) in big.iter() {
                if *sz == 0 {
                    continue;
                }
                if shown >= OOM_TOP_SITES {
                    break;
                }
                shown += 1;
                emit(format_args!("  {:>5} KiB <-", sz >> 10));
                for f in site.iter() {
                    if *f != 0 {
                        emit(format_args!(" {}", kernel_hal::ksyms::Addr(*f as u64)));
                    }
                }
                emit(format_args!("\n"));
            }
            if big_missed > 0 {
                emit(format_args!(
                    "  ({} more were not tracked: the table holds 128 live blocks)\n",
                    big_missed,
                ));
            }
        }
        let (sites, missed, scale) = crate::memory::heap_alloc_sites();
        if sites.iter().any(|(live, _, _)| *live > 0) {
            emit(format_args!(
                "live blocks by allocating call site (1 in {} sampled, so x{}):\n",
                scale, scale,
            ));
            let mut shown = 0usize;
            for (live, bytes, frames) in sites {
                if live == 0 {
                    continue;
                }
                if shown >= OOM_TOP_SITES {
                    break;
                }
                shown += 1;
                emit(format_args!(
                    "  ~{:>6} KiB in ~{} blocks <-",
                    (bytes * scale) >> 10,
                    live * scale,
                ));
                // Two frames, not four: a symbolised frame is most of a
                // screen line, and four of them wrap one site over three
                // rows -- which on this console costs two other sites.
                for f in frames.iter().take(OOM_SITE_FRAMES) {
                    if *f != 0 {
                        emit(format_args!(" {}", kernel_hal::ksyms::Addr(*f as u64)));
                    }
                }
                emit(format_args!("\n"));
            }
            if missed > 0 {
                emit(format_args!("  ({} samples found no slot)\n", missed));
            }
        } else {
            emit(format_args!(
                "call sites: no sampled block is live (sampling arms at the first \
                 heap-pressure level)\n"
            ));
        }
    }
    // The used/total line again, LAST. It is printed first, and first is what
    // a wrapping console overwrites first; it is also the one number that says
    // whether this is a leak or a kernel whose heap was never big enough.
    emit(format_args!(
        "kernel OOM: alloc {} bytes failed (used {} / total {} MiB)\n",
        layout.size(),
        heap_used / 1024 / 1024,
        heap_total / 1024 / 1024,
    ));
    panic!("memory allocation of {} bytes failed", layout.size());
}

/// Does `sym` name a frame of the kernel's own fault/panic diagnosis path?
///
/// Asked of a HOLDER's `is now at` symbol, and it decides the banner's
/// verdict. A lock whose holder faulted inside its critical section is never
/// released, so every other cpu piles up behind it and the report reads
/// exactly like a cycle -- which is how a convoy of waiters gets chased as an
/// AB-BA that was never there. When the holder is sitting in
/// `handle_page_fault`, the deadlock is not the bug; the fault printed above
/// it is. [`self_deadlock_cpu`] already calls the case where the holder is
/// *also* a waiter; this one is the commoner shape, where the faulted holder
/// never comes back for the lock at all.
///
/// Matched on substrings, because these frames are generic and mangled
/// (`<zcore::handler::ZcoreKernelHandler as kernel_hal::..>::handle_page_fault`)
/// and `ksyms` hands back one folded name per address.
///
/// Narrow on purpose, and two names are the reason the test for it is as long
/// as it is:
///
///  * `handle_page_fault` alone is not the kernel handler. `zircon-object`
///    has two of its own (`vm::vmar` and the guest's), and a holder sitting in
///    those is taking a fault its vmar RESOLVES -- slow, not fatal. So that
///    needle only counts paired with the zCore handler that cannot resolve it.
///  * `panic_banner` is not here at all, though it looks like it belongs: the
///    deadlock reporter below calls it itself, so a peer NMI landing there
///    would have the renderer diagnose itself. The panic frames that do count
///    (`rust_begin_unwind`, the hook) are above it anyway.
pub(crate) fn sym_is_diagnosing(sym: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "handle_trap",
        "trap_handler",
        "report_unresolved_kernel_fault",
        "print_fault_backtrace",
        "oops::try_contain",
        "rust_begin_unwind",
        "panic_handler",
        "panic_fmt",
    ];
    // The zCore kernel handler's own page-fault entry, whichever way `ksyms`
    // folded the generic: the impl type or the trait it implements.
    if sym.contains("handle_page_fault")
        && (sym.contains("zcore::handler") || sym.contains("kernel_handler::KernelHandler"))
    {
        return true;
    }
    NEEDLES.iter().any(|n| sym.contains(n))
}

/// How many of a HOLDER's callers the banner prints under its `is now at`
/// line.
///
/// Three is what separates the two causes this banner cannot otherwise tell
/// apart. A heap `dealloc` reached from `eclipsefs`'s write is three frames
/// from the lock's acquire site; a `dealloc` that is merely the top of a
/// convoy is not. Beyond three the chain stops discriminating and starts
/// spending the verdict's budget -- see [`BANNER_BYTES`].
///
/// Only the x86_64 banner walks a chain (the NMI probe that captures the
/// frame pointer is x86_64's), so on the other architectures this and
/// [`walk_frame_chain`] would be dead code and the build denies warnings.
#[cfg(any(target_arch = "x86_64", test))]
pub(crate) const HOLDER_CHAIN_FRAMES: usize = 3;

/// Walk a frame-pointer chain and collect up to `out.len()` return addresses.
///
/// Pure, so the whole of it is testable on the host: `read` fetches one qword,
/// `stack_ok` says whether an address may be read as a stack qword at all, and
/// `text_ok` says whether a word is a return address worth printing. The
/// bare-metal caller passes a volatile read and the `kaddr` predicates; a test
/// passes a synthetic stack.
///
/// The chain being walked belongs to ANOTHER cpu -- a wedged one, which is why
/// its frames are stable enough to read -- so every step is guarded and the
/// walk gives up rather than chase a word it cannot vouch for:
///
///  * `rbp` must be a readable stack qword, or there is nothing to read;
///  * frames grow downward, so a `next` that does not increase is corrupt and
///    would otherwise be a loop the painter never leaves;
///  * a return address outside the kernel's text is not a caller, and
///    printing it would invite `addr2line` on a number that means nothing.
///
/// Returns how many entries of `out` were filled.
#[cfg(any(target_arch = "x86_64", test))]
pub(crate) fn walk_frame_chain(
    mut rbp: u64,
    read: impl Fn(u64) -> u64,
    stack_ok: impl Fn(u64) -> bool,
    text_ok: impl Fn(u64) -> bool,
    out: &mut [u64],
) -> usize {
    let mut n = 0;
    while n < out.len() {
        if !stack_ok(rbp) || !stack_ok(rbp + 8) {
            break;
        }
        let next = read(rbp);
        let ret = read(rbp + 8);
        if !text_ok(ret) {
            break;
        }
        out[n] = ret;
        n += 1;
        if next <= rbp {
            break; // stack grows down; a non-increasing frame is corrupt
        }
        rbp = next;
    }
    n
}

/// [`walk_frame_chain`] over a wedged peer's real stack, with the kernel's own
/// guards. Zero frames whenever the NMI brought back no usable frame pointer
/// (it never landed, or it landed in ring 3).
#[cfg(target_arch = "x86_64")]
fn holder_chain(rbp: u64, out: &mut [u64; HOLDER_CHAIN_FRAMES]) -> usize {
    if rbp == 0 {
        return 0;
    }
    walk_frame_chain(
        rbp,
        |a| unsafe { core::ptr::read_volatile(a as *const u64) },
        kernel_hal::kaddr::is_kernel_stack_qword,
        kernel_hal::kaddr::is_kernel_text,
        out,
    )
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
pub(crate) const BANNER_BYTES: usize = 2048;

/// Bytes [`StackBuf::with_reserve`] holds back for the deadlock banner's final
/// verdict plus the `[+N B cut]` marker: the longest `DIAG:` line is ~190
/// bytes and the marker ~20.
const VERDICT_RESERVE: usize = 352;

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

/// The cpu that holds a lock it is itself waiting for, if any.
///
/// Keys are `(file ptr, packed line|cpu)` -- the whole recorded site, not just
/// the cpu. **Matching on the cpu alone would call every AB-BA a
/// self-deadlock**: in a two-lock cycle each cpu is legitimately a holder of
/// one lock and a waiter for the other, so both appear on both lists. What
/// cannot happen without re-entry is one cpu recorded as holder AND waiter at
/// the *same acquire site*: a ticket mutex is not re-entrant and every guard
/// disables interrupts, so no *interrupt* can produce it. A fault or a panic
/// taken inside the critical section can, and that is the whole finding -- the
/// handler came back into a lock its own cpu holds, and waits with interrupts
/// off for a release only it could perform.
///
/// It is also the one shape the old verdict could not be: "either a
/// lock-ordering cycle (AB-BA) or a HOLDER stuck inside its own critical
/// section" sent the reader hunting a second lock that does not exist. The
/// framebuffer (`shadow_fb`) and the object table both produced exactly this
/// banner before they grew a re-entrancy escape; the serial console produced
/// it on the bring-up box, from `uart_16550.rs:249`, with `cpu=18` on both
/// lines.
fn self_deadlock_cpu(holders: &[(usize, usize)], waiters: &[(usize, usize)]) -> Option<usize> {
    holders
        .iter()
        .find(|key| waiters.contains(key))
        .map(|(_, lc)| dl_cpu(*lc))
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
/// Does the verdict `dl_paint` is about to print say that a fault or panic was
/// already reported, and that THIS banner is only its consequence?
///
/// It mirrors the branch order of the verdict chain, and that order is the
/// whole point: `diagnosing_cpu` can be set on a run whose verdict is
/// shootdown starvation, and there nothing has been printed to preserve.
/// Reading the flag on its own would suppress the one banner that is itself
/// the finding.
pub(crate) fn verdict_follows_a_printed_fault(
    self_deadlock: bool,
    shootdown: bool,
    diagnosing: bool,
) -> bool {
    if self_deadlock {
        // "A fault or panic was taken inside that critical section and the
        // handler came back in" -- the re-entry is the bug, and its report is
        // what is underneath.
        true
    } else if shootdown {
        false
    } else {
        diagnosing
    }
}

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
    // The full recorded site of every holder and of every waiter -- not just
    // the cpu, which an AB-BA cycle puts on both lists too. See
    // `self_deadlock_cpu`.
    let mut holder_keys = [(0usize, 0usize); DL_SLOTS];
    let mut holder_keys_n = 0usize;
    let mut waiter_keys = [(0usize, 0usize); DL_SLOTS];
    let mut waiter_keys_n = 0usize;
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
            holder_keys[holder_keys_n] = (p, lc);
            holder_keys_n += 1;
        }
        if !is_holder && waiter_keys_n < DL_SLOTS {
            waiter_keys[waiter_keys_n] = (p, lc);
            waiter_keys_n += 1;
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
    let reporting = panicking_cpu();
    // Every RECORDED holder, not just the two the banner has room to print:
    // the verdict must not depend on which holders fit on the screen. With a
    // three-cpu cycle, or an earlier holder still in its slot, the one that
    // faulted can sit third in this list.
    let mut diagnosing_cpu: Option<usize> = None;
    for &c in holder_cpus.iter().take(holder_n) {
        if c == reporting {
            continue;
        }
        let rip = kernel_hal::kstats::nmi_rip(c);
        if rip != 0 {
            if let Some((sym, _)) = kernel_hal::ksyms::lookup(rip) {
                if sym_is_diagnosing(sym) {
                    diagnosing_cpu = Some(c);
                    break;
                }
            }
        }
    }
    for &c in holder_cpus.iter().take(holder_n.min(2)) {
        let rip = kernel_hal::kstats::nmi_rip(c);
        if c == reporting {
            // `capture_cpu_rips` broadcasts to all OTHER cpus -- this one never
            // gets an NMI, so its slot holds whatever an EARLIER broadcast left
            // there. On the capture this guard comes from that was
            // `hal_cpu_idle+0xd0`, read off a photograph as "the holder went to
            // sleep holding the lock" and chased as such; the cpu was in fact
            // right here, printing this banner. The one line the DIAG verdict
            // tells the reader to act on must not be the stale one.
            let _ = write!(
                b,
                "\nHOLDER cpu{} IS THE CPU PRINTING THIS -- it is in the acquire \
                 above, waiting on itself (no self-NMI, so no RIP)",
                c
            );
        } else if rip != 0 {
            let _ = write!(
                b,
                "\nHOLDER cpu{} is now at {}",
                c,
                kernel_hal::ksyms::Addr(rip)
            );
            // …and who called it. The top frame alone reads the same whether
            // the holder is merely slow down there or wedged under a second
            // lock; the chain is what the reader acts on. Same NMI that
            // brought back the RIP, so no extra broadcast.
            #[cfg(target_arch = "x86_64")]
            {
                let mut frames = [0u64; HOLDER_CHAIN_FRAMES];
                let n = holder_chain(kernel_hal::kstats::nmi_rbp(c), &mut frames);
                for ret in frames.iter().take(n) {
                    let _ = write!(b, "\n   called from {}", kernel_hal::ksyms::Addr(*ret));
                }
            }
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
    // What the disk is doing, which is the other half of the question the
    // verdict asks. A command issued from under the filesystem's lock runs
    // with interrupts off and is allowed ten seconds, eight times over, so a
    // disk that is merely slow produces this banner with nothing wrong in the
    // software at all. One line says which: a command in flight and for how
    // long, plus the timeouts and retries behind it.
    {
        let (cur, timeouts, retries) = kernel_hal::drivers::disk_inflight();
        match cur {
            Some((port, what, us)) => {
                let _ = write!(
                    b,
                    "\nDISK: {} in flight on port {} for {} ms",
                    what,
                    port,
                    us / 1000
                );
            }
            None => {
                let _ = write!(b, "\nDISK: no command in flight");
            }
        }
        let _ = write!(
            b,
            " ({} timeouts, {} retries since boot)",
            timeouts, retries
        );
        // What the instant above cannot say: whether the eight seconds the
        // waiter has just spent were the disk working or the CPU. A holder
        // that was never waiting on the disk leaves this near zero.
        let (busy_us, done) = kernel_hal::drivers::disk_busy_over_last(8);
        let _ = write!(
            b,
            "\nDISK: busy {} ms of the last 8 s over {} commands",
            busy_us / 1000,
            done
        );
    }
    // Everything above was allowed to be cut; what follows is not. The verdict
    // is the line a reader acts on, so it gets the bytes held back for it.
    b.release_reserve();
    // One-line verdict so the on-screen (no-serial) capture is self-diagnosing.
    let self_deadlock =
        self_deadlock_cpu(&holder_keys[..holder_keys_n], &waiter_keys[..waiter_keys_n]);
    if let Some(cpu) = self_deadlock {
        let _ = write!(
            b,
            "\nDIAG: self-deadlock on cpu{} -- it HOLDS the lock it is waiting \
             for. A fault or panic was taken inside that critical section and \
             the handler came back in. Not AB-BA: the re-entry is the bug.",
            cpu
        );
    } else if shootdown_head {
        let _ = write!(
            b,
            "\nDIAG: shootdown starvation — the HOLDER waits a TLB ack from a CPU \
             that never pumps; symbolize the non-acker nmi_rip above to name it. \
             Not AB-BA."
        );
    } else if let Some(faulted) = diagnosing_cpu {
        // The case the either/or below cannot call, and the one the captures
        // keep showing: a HOLDER sitting in the kernel's own fault path, i.e.
        // it faulted with the lock held and will never give it back. The
        // discriminator was already on the screen -- the `is now at` symbol --
        // but reading it asked the reader to know which symbols are the fault
        // path, which is exactly what a photograph of a wedged machine does
        // not help with.
        // The cpu is named here because it may be a holder the banner had no
        // room to print, and then this line is the only mention of it.
        let _ = write!(
            b,
            "\nDIAG: HOLDER cpu{} faulted or panicked INSIDE its critical section (it \
             is in the kernel's own fault/panic path), so that lock is never given \
             back. Not AB-BA: this report is a CONSEQUENCE of the fault printed above \
             it, not a second bug. Diagnose that fault.",
            faulted
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
    // Whether this report is a CONSEQUENCE of a fault some cpu has already
    // printed -- the verdicts that end in "Diagnose that fault." and "the
    // re-entry is the bug".
    //
    // When it is, this banner must not reach the framebuffer. The glass is
    // the only artifact a real machine gives back, the stop-screen writes
    // over whatever is under it, and what is under it here is the very fault
    // the verdict is pointing at. A capture from 16-oct-2026 lost exactly
    // that: an `alloc_error` panic on cpu0 inside `eclipsefs_mount.rs:512`,
    // whose header named `lang.rs:111` and the size that could not be
    // allocated, came back as `KEDEADLOCK` and `mecpu=1` -- the banner drawn
    // through it, one glyph cell at a time. Four copies of a report that says
    // "this is not the bug" cost the reader the line that was.
    //
    // Serial still gets the whole thing: it scrolls, so nothing is destroyed
    // there, and a headless run has nothing else.
    let consequence_of_a_fault = verdict_follows_a_printed_fault(
        self_deadlock.is_some(),
        shootdown_head,
        diagnosing_cpu.is_some(),
    );
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
        // The framebuffer repaint is now behind this same gate, not run on
        // every call. It used to repaint unconditionally, so a convoy of
        // waiters drew the banner once per cpu that piled up -- the capture
        // above came back with four near-identical copies, each one more of
        // the screen spent on the same report. The gate loses nothing: the
        // reason the repaint exists at all is that the first paint happens
        // before the waiter has snapshotted the HOLDER, and gaining that is
        // gaining a site.
        if !consequence_of_a_fault {
            kernel_hal::console::panic_banner(valid);
        }
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

/// How many panic reports are in flight on this machine, and which CPUs are
/// already inside one.
///
/// Two different shapes arrive here and they need different answers.
///
/// **The same CPU re-entering.** The handler below prints a banner, a red
/// framebuffer band, the message, up to 32 backtrace lines and the exception
/// summary; any one of those touches memory the fault being reported may have
/// already wrecked. When one of them faults, the trap handler panics about THAT
/// fault, the handler starts over, and the machine spends the rest of its life
/// repainting a loop of reports instead of keeping the first one -- which is
/// the real one -- on the glass. Nothing that CPU says about the second panic
/// is worth the report it would erase, so it gets its location and nothing
/// else.
///
/// **Another CPU panicking at the same moment.** Its report is not a loop and
/// its message is evidence -- `index out of bounds: the len is 0 but the index
/// is 4` is the whole diagnosis, and it belongs to a different fault than the
/// one being reported. What makes a five-CPU pile-up unreadable is not the
/// messages, it is five banners, five framebuffer bands and five backtraces
/// interleaving line by line until no message sits under its own location.
/// So a peer prints one line, with its message, and stops.
///
/// `PANIC_DEPTH` is reset to zero immediately before containment, which does
/// not return when it succeeds: a fault the kernel contained and survived must
/// not leave the counter armed, or the next genuine panic -- minutes later,
/// about something else entirely -- would print one line and halt.
static PANIC_DEPTH: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Bit `i` set: logical CPU `i` is already inside the panic handler. Separate
/// from the depth because the depth cannot tell a loop from a pile-up.
static PANICKING_CPUS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Hard cap on reports, counting the full one. A one-line path that faults
/// inside its own single line would otherwise loop just as happily as the full
/// report did; past this many the handler says nothing at all. Generous enough
/// that a pile-up on a 16-thread desktop still prints every CPU's message.
const PANIC_REPORT_LIMIT: usize = 24;

/// What a panic arriving at the handler should do.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum PanicEntry {
    /// Nothing else is being reported: print everything.
    Report,
    /// Another CPU is reporting. One line with this panic's location AND its
    /// message -- a different fault, so its message is new evidence -- and then
    /// halt, without the banner, the band, the backtrace or containment.
    Peer,
    /// This CPU is already inside a report: its own reporting path faulted.
    /// Location only; formatting `info.message()` is one of the ways the first
    /// report falls over, and the first report is the one that matters.
    Nested,
    /// Past the cap. Say nothing and halt; anything else is a loop.
    Silent,
}

/// The rule, apart from the handler so a test can run it.
///
/// `depth` is the number of reports that were already in flight, and
/// `same_cpu` whether this CPU was already one of them.
pub(crate) fn panic_entry(depth: usize, same_cpu: bool) -> PanicEntry {
    if depth == 0 {
        PanicEntry::Report
    } else if depth >= PANIC_REPORT_LIMIT {
        PanicEntry::Silent
    } else if same_cpu {
        PanicEntry::Nested
    } else {
        PanicEntry::Peer
    }
}

/// Which CPU is reporting, for the bitmap only.
///
/// Resolved from the Local APIC id rather than through `cpu::cpu_id()`, which
/// reads the GS-backed per-CPU region. A GS that is lying -- a corrupted
/// publisher, or the `syscall_return` window where the USER gsbase is already
/// swapped in while CS is still ring 0 -- yields a perfectly valid-looking
/// logical id belonging to somebody else, which would set a peer's bit and make
/// this CPU's own re-entry read as a peer panic: the one classification this
/// guard exists to get right. `kstats::fault_slot` keys its per-CPU fault slots
/// off the same resolver, for the same reason.
///
/// [`lock::NO_CPU`] (255) -- a hardware id that resolves to no registered CPU --
/// deliberately falls outside the bitmap rather than folding onto the boot CPU.
fn panicking_cpu() -> usize {
    lock::current_cpu_id_via_apic() as usize
}

/// Mark this CPU as reporting and say whether it already was.
///
/// A cpu id the bitmap cannot hold is never treated as "already panicking":
/// answering "no" there costs at most one extra line, while answering "yes"
/// would silence a report.
/// Whether any CPU is inside the panic handler right now.
///
/// Asked by the diagnostic reporters that run on the OTHER CPUs. A panic does
/// not stop them: they keep running, and whatever they print lands on top of
/// the report the panicking CPU is writing -- on a console with one page and
/// no scrollback, that is the report lost. The counters they keep are printed
/// by the OOM report itself, so standing down costs nothing.
#[cfg(any(all(target_arch = "x86_64", not(feature = "libos")), test))]
pub(crate) fn any_cpu_panicking() -> bool {
    PANICKING_CPUS.load(core::sync::atomic::Ordering::SeqCst) != 0
}

#[cfg(not(test))]
fn mark_panicking(cpu: usize) -> bool {
    use core::sync::atomic::Ordering;
    if cpu >= 64 {
        return false;
    }
    let bit = 1u64 << cpu;
    PANICKING_CPUS.fetch_or(bit, Ordering::SeqCst) & bit != 0
}

/// Release the guard, for the one caller that may legitimately leave the
/// handler running: [`crate::oops::try_contain`], at the point where it has
/// committed to abandoning the coroutine and will not return.
///
/// The reset cannot happen *before* that handoff. Clearing the depth while this
/// CPU's bit is still set makes the next `fetch_add` answer 0, so a fault
/// inside containment reads as `Report` and runs the whole banner and backtrace
/// again, over the report the guard exists to keep. Clearing both, at the point
/// after which nothing of this panic runs, is the only moment that is neither
/// too early nor never.
///
/// A containment that *declines* returns into the halt below and needs no
/// reset: that CPU is not coming back.
pub(crate) fn release_panic_guard() {
    use core::sync::atomic::Ordering;
    // Resolved the same way `mark_panicking` set it. Taking the caller's own
    // notion of which CPU it is would risk clearing a peer's bit: `try_contain`
    // keys its own guard off `cpu::cpu_id()`, and the whole reason this bitmap
    // uses the APIC resolver is that the two can disagree.
    let cpu = panicking_cpu();
    if cpu < 64 {
        PANICKING_CPUS.fetch_and(!(1u64 << cpu), Ordering::SeqCst);
    }
    PANIC_DEPTH.store(0, Ordering::SeqCst);
}

/// Write the panic header -- who panicked, which build, where, and what it
/// said -- into `b`.
///
/// Shared by the two places that print it, which is the point: the stop-screen
/// banner at the top of the handler and the reprint at the very bottom, after
/// the backtrace. A reader comparing the top of a photograph with the bottom
/// has to see the same line, not two that drifted apart.
///
/// `cpu` and `message` are taken by `Display` rather than by their concrete
/// types: `cpu_id()` is a `u8` on bare metal and a `usize` under libos, and
/// `PanicInfo::message` hands back a `PanicMessage`, not the `Arguments` it
/// used to be. Naming any one of those here would tie this to one build.
pub(crate) fn write_panic_header(
    b: &mut StackBuf,
    cpu: impl core::fmt::Display,
    build: &str,
    loc: Option<(&str, u32)>,
    message: impl core::fmt::Display,
) {
    use core::fmt::Write;
    match loc {
        Some((file, line)) => {
            let _ = write!(
                b,
                "KERNEL PANIC cpu={} [build {}] {}:{}\n{}",
                cpu, build, file, line, message
            );
        }
        None => {
            let _ = write!(b, "KERNEL PANIC cpu={} [build {}]\n{}", cpu, build, message);
        }
    }
}

/// Stop this CPU for good. The halt tail of the panic handler, shared with the
/// short paths so they cannot drift apart.
#[cfg(not(test))]
fn panic_halt() -> ! {
    if cfg!(feature = "baremetal-test") {
        kernel_hal::cpu::reset();
    }
    loop {
        core::hint::spin_loop();
    }
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

    // Before anything that could fault: is somebody already reporting, and is
    // it this CPU? See [`PANIC_DEPTH`].
    let reporting_cpu = panicking_cpu();
    let same_cpu = mark_panicking(reporting_cpu);
    let entry = panic_entry(
        PANIC_DEPTH.fetch_add(1, core::sync::atomic::Ordering::SeqCst),
        same_cpu,
    );
    if entry != PanicEntry::Report {
        // Deliberately the least machinery that can still say something: no
        // framebuffer rasterizer, no backtrace walk, no console mode change, no
        // allocation, no containment.
        //
        // **Serial only.** Both arms below used to write the graphic console
        // too, which is a framebuffer rasterizer -- the first thing this
        // comment rules out -- reached through a `dyn DisplayScheme` fat
        // pointer, and these arms run *before* `note_panicking()` tells that
        // console to stop trusting its cell cache. A capture from hardware
        // shows what that costs: `[#UD] RIP 0xa0002` and `[panic-nested]
        // trap.rs:1179` alternating fifteen times, because the nested arm's own
        // graphic write faulted, which panicked, which re-entered here. An
        // exception is not masked by the `intr_off()` above, so nothing stopped
        // it but `PANIC_REPORT_LIMIT` -- a backstop doing a fix's job, 24 deep,
        // with the real report buried above the noise.
        //
        // Nothing is lost on a screen-only machine: the full report that ran
        // first already painted the stop screen, and these arms exist to say
        // that *it* is the one to read.
        let (file, line) = match info.location() {
            Some(l) => (l.file(), l.line()),
            None => ("<unknown>", 0),
        };
        match entry {
            PanicEntry::Peer => {
                let args = format_args!(
                    "\n[panic-peer] cpu={} also panicked at {}:{} while another \
                     CPU's report was printing: {}\n",
                    kernel_hal::cpu::cpu_id(),
                    file,
                    line,
                    info.message(),
                );
                kernel_hal::console::serial_write_fmt_spin(args);
            }
            PanicEntry::Nested => {
                let args = format_args!(
                    "\n[panic-nested] cpu={} panicked again at {}:{} while \
                     printing its own report. THE REPORT ABOVE IS THE REAL ONE; \
                     this is its reporting path falling over.\n",
                    kernel_hal::cpu::cpu_id(),
                    file,
                    line,
                );
                kernel_hal::console::serial_write_fmt_spin(args);
            }
            PanicEntry::Silent | PanicEntry::Report => {}
        }
        panic_halt();
    }

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
        let mut b = StackBuf::new();
        // The build id, for the same reason the deadlock banner carries one: a
        // stale build booted after a fix landed reads exactly like the fix not
        // working, and a photographed stop screen is all anybody has.
        write_panic_header(
            &mut b,
            kernel_hal::cpu::cpu_id(),
            env!("ECLIPSE_BUILD_ID"),
            info.location().map(|l| (l.file(), l.line())),
            info.message(),
        );
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
    let prev_kd = kd_restore_target(
        PRE_REPORT_KD.load(core::sync::atomic::Ordering::SeqCst),
        kernel_hal::console::kd_mode(),
    );
    claim_text_console();

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

    // The last word on the screen. The early framebuffer console does not
    // scroll: it clears and restarts at the top when it fills, so on a 25-line
    // display the backtrace just printed above can have wiped the panic
    // message -- vector, symbolized RIP, error code and all -- off the glass.
    // A photo of the screen is the only artifact that comes back from a real
    // machine, so the one line that has to survive is printed last, after
    // everything that could overwrite it. Armed by the x86 trap handler
    // immediately before it panics, and taken rather than peeked so an
    // ordinary panic is never decorated with an exception it did not take.
    #[cfg(target_arch = "x86_64")]
    if let Some((vec, error_code, rip)) = kernel_hal::kstats::take_exception() {
        let summary = format_args!(
            "[exception] {} (vec={:#x}) at rip={}\n[exception] {}\n",
            kernel_hal::context::x86_vector_name(vec),
            vec,
            kernel_hal::ksyms::Addr(rip),
            kernel_hal::context::X86TrapErrorCode { vec, error_code },
        );
        kernel_hal::console::serial_write_fmt_spin(summary);
        kernel_hal::console::graphic_console_write_fmt_spin(summary);
    }

    // The panic message itself, printed again as the last thing on the glass.
    //
    // The framebuffer console does not scroll: it clears and restarts at the
    // top when it fills (see the comment above the backtrace). The handler
    // prints the message first and up to 32 frames after it, so on a deep
    // stack the backtrace wipes the message -- and the stop-screen banner
    // rasterized at the top of this function is painted over by the same text
    // console. A photographed screen then carries a tail of return addresses
    // and nothing that says what panicked or on which build: one came back
    // from a real machine showing six frames of the generic syscall spine and
    // no message at all. The `[exception]` line above is printed last for this
    // reason already; the message needs it just as much, and more, because a
    // `panic!` that took no CPU exception leaves that line unprinted.
    //
    // Also recorded in the oops ring. The handler never wrote there, so a
    // panic `oops::try_contain` goes on to contain -- the machine survives and
    // the screen scrolls away -- left nothing behind in `/proc/oops` or
    // `/var/log/oops.log` either. `record` is a lock-free byte ring and
    // allocates nothing, so it is safe on this path.
    {
        let mut b = StackBuf::new();
        write_panic_header(
            &mut b,
            kernel_hal::cpu::cpu_id(),
            env!("ECLIPSE_BUILD_ID"),
            info.location().map(|l| (l.file(), l.line())),
            info.message(),
        );
        let tail = format_args!("\n{}\n", b.valid_str());
        kernel_hal::console::serial_write_fmt_spin(tail);
        kernel_hal::console::graphic_console_write_fmt_spin(tail);
        kernel_hal::oops_log::record(tail);
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
    // No reset here: see [`release_panic_guard`]. `try_contain` clears the
    // guard itself, at the point where it has committed to abandoning the
    // coroutine and will not return, so a fault *inside* containment is still
    // caught as the re-entry it is.
    if !cfg!(feature = "baremetal-test") {
        crate::oops::try_contain("kernel panic", Some(prev_kd));
    }

    panic_halt()
}

/// The banner builder and the deadlock slots, on the host.
///
/// Neither had ever been compiled by a test binary: this module is
/// `#[cfg(not(feature = "libos"))]` in the kernel build and `cargo test -p
/// zcore` runs with `--features libos`, so the panic path and the deadlock
/// report were written, twice rewritten, and never once executed by a job.
/// Only `#[panic_handler]` and `#[alloc_error_handler]` genuinely cannot be
/// here (std supplies both); everything below is the code the machine runs.

/// Which mode the VT goes back to when a fault is contained and the machine
/// runs on. The screen is claimed before the report is written, so the live
/// mode by then is the reporter's own `KD_TEXT` and restoring it would strand
/// a compositor that is still alive.
#[cfg(test)]
mod kd_restore_tests {
    use super::{kd_restore_target, KD_UNCLAIMED};

    const KD_TEXT: u32 = 0;
    const KD_GRAPHICS: u32 = 1;

    #[test]
    fn with_nobody_claiming_the_live_mode_is_what_gets_restored() {
        // The plain panic: no reporter ran ahead of the handler, so the mode
        // read a line earlier is still the one the machine was in.
        assert_eq!(kd_restore_target(KD_UNCLAIMED, KD_GRAPHICS), KD_GRAPHICS);
        assert_eq!(kd_restore_target(KD_UNCLAIMED, KD_TEXT), KD_TEXT);
    }

    #[test]
    fn a_claim_wins_over_the_live_mode() {
        // The OOM path: the reporter already forced KD_TEXT, so the live
        // value is its own doing. Restoring it would leave a live labwc
        // rendering into a buffer nothing presents -- a desktop that looks
        // frozen although only one process died.
        assert_eq!(kd_restore_target(KD_GRAPHICS, KD_TEXT), KD_GRAPHICS);
    }

    #[test]
    fn a_claim_taken_while_already_in_text_restores_text() {
        // Nothing to put back, and nothing to get wrong: the claim recorded
        // KD_TEXT because that is where the screen was.
        assert_eq!(kd_restore_target(KD_TEXT, KD_TEXT), KD_TEXT);
    }

    #[test]
    fn the_sentinel_is_not_a_mode_any_vt_can_be_in() {
        // `normalize_kd_mode` accepts 0..=3 and nothing else, so no VT can
        // ever hold the sentinel and be mistaken for unclaimed.
        assert!(KD_UNCLAIMED > 3);
    }
}

/// The OOM report's ranking, which decides what a photograph of a dead machine
/// says. It is called from the `#[alloc_error_handler]`, which `cargo test`
/// cannot run at all, so it is a free function with its own tests rather than
/// a loop inside the handler.
#[cfg(test)]
mod oom_ranking_tests {
    use super::top_rows_by_bytes;

    #[test]
    fn rows_come_back_heaviest_first_by_bytes_not_by_count() {
        // 1 block of 1 MiB outweighs 100 blocks of 16 B, and the old report,
        // which printed the classes in index order, buried it under them.
        let (top, more, more_bytes) =
            top_rows_by_bytes::<4>([(16, 100), (1 << 20, 1), (4096, 2)].into_iter());
        assert_eq!(top[0], (1 << 20, 1));
        assert_eq!(top[1], (4096, 2));
        assert_eq!(top[2], (16, 100));
        assert_eq!(top[3], (0, 0));
        assert_eq!((more, more_bytes), (0, 0));
    }

    #[test]
    fn what_does_not_fit_is_counted_and_its_bytes_added_up() {
        let (top, more, more_bytes) =
            top_rows_by_bytes::<2>([(100, 1), (200, 1), (300, 1), (400, 1)].into_iter());
        assert_eq!(top, [(400, 1), (300, 1)]);
        assert_eq!(more, 2);
        assert_eq!(more_bytes, 100 + 200);
    }

    /// A row that arrives late and outranks one already in the table pushes it
    /// out — and the pushed-out row is then one of the omitted, not a row that
    /// vanished from both the list and the count.
    #[test]
    fn a_row_evicted_by_a_heavier_one_is_counted_as_omitted() {
        let (top, more, more_bytes) =
            top_rows_by_bytes::<2>([(10, 1), (20, 1), (30, 1)].into_iter());
        assert_eq!(top, [(30, 1), (20, 1)]);
        assert_eq!((more, more_bytes), (1, 10));
    }

    /// The tables these read are fixed arrays with empty slots. An empty slot
    /// is not a class that was left out.
    #[test]
    fn empty_slots_are_neither_shown_nor_counted() {
        let (top, more, more_bytes) =
            top_rows_by_bytes::<3>([(0, 0), (64, 2), (0, 5), (8, 0)].into_iter());
        assert_eq!(top[0], (64, 2));
        assert_eq!(top[1], (0, 0));
        assert_eq!((more, more_bytes), (0, 0));
    }

    #[test]
    fn an_empty_table_reports_nothing_rather_than_a_row_of_zeroes() {
        let (top, more, more_bytes) = top_rows_by_bytes::<3>(core::iter::empty());
        assert_eq!(top, [(0, 0); 3]);
        assert_eq!((more, more_bytes), (0, 0));
    }

    /// Every row the handler can print costs a screen line, and the budget is
    /// one screen: the report has to leave room for the panic banner and the
    /// backtrace under it. This pins the headline number of that budget.
    #[test]
    fn the_report_cannot_grow_past_what_one_screen_holds() {
        // Two capped tables of rows and two of call sites, each with a
        // header and a "+N more" line, plus the fixed lines (the used/total
        // line at each end, memfd, and the call-site note) and the panic
        // banner + backtrace under all of it.
        let report_lines = 2 * (super::OOM_TOP_ROWS + 2) + 2 * (super::OOM_TOP_SITES + 2) + 4;
        let backtrace_lines = 14;
        // 1024x768 — the smallest mode the early console is ever handed —
        // is 48 rows of the 16-pixel font.
        assert!(
            report_lines + backtrace_lines <= 48,
            "{} lines",
            report_lines + backtrace_lines
        );
    }

    /// Counted as the handler prints them, because a site line that wraps
    /// costs two rows of the budget above and the frame cap is what holds it
    /// to one.
    #[test]
    fn a_call_site_line_prints_at_most_two_symbolised_frames() {
        assert_eq!(super::OOM_SITE_FRAMES, 2);
    }
}

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

    // ── The panic pile-up guard ─────────────────────────────────────────────

    /// The first report is the real one, so it is the only one that gets the
    /// whole machinery. Without this the handler re-entered itself for as long
    /// as the machine was alive, repainting a loop of reports over the fault
    /// that started it.
    #[test]
    fn only_the_first_panic_in_flight_gets_the_full_report() {
        assert_eq!(panic_entry(0, false), PanicEntry::Report);
        assert_eq!(panic_entry(0, true), PanicEntry::Report);
    }

    /// A CPU that re-enters its own report is a loop: everything it would say
    /// about the second panic costs the first report, which is the one that
    /// matters. Location only, and never `info.message()` -- a corrupt
    /// `Arguments` is one of the ways the first report falls over.
    #[test]
    fn a_cpu_that_panicked_inside_its_own_report_gets_its_location_only() {
        assert_eq!(panic_entry(1, true), PanicEntry::Nested);
        assert_eq!(panic_entry(5, true), PanicEntry::Nested);
    }

    /// A *peer* is not a loop, and its message is a different fault's evidence.
    /// The photograph of 2026-10-04 had five CPUs down at once and the only
    /// line worth anything on it was one of their messages -- `index out of
    /// bounds: the len is 0 but the index is 4` -- so suppressing peers'
    /// messages would have thrown away the diagnosis to tidy the screen.
    #[test]
    fn a_peer_cpu_keeps_its_message() {
        assert_eq!(panic_entry(1, false), PanicEntry::Peer);
        assert_eq!(panic_entry(4, false), PanicEntry::Peer);
    }

    /// The cap is what makes the short paths safe: a one-line path that faults
    /// inside its own single line would loop exactly as the full report did.
    #[test]
    fn past_the_cap_nothing_is_printed_at_all() {
        assert_eq!(panic_entry(PANIC_REPORT_LIMIT, false), PanicEntry::Silent);
        assert_eq!(panic_entry(PANIC_REPORT_LIMIT, true), PanicEntry::Silent);
        assert_eq!(panic_entry(usize::MAX, false), PanicEntry::Silent);
    }

    /// And the cap has to leave room for every CPU of a machine this kernel
    /// runs on to say its line, or a pile-up loses the tail of itself.
    #[test]
    fn the_cap_leaves_room_for_a_whole_desktops_worth_of_cpus() {
        assert!(PANIC_REPORT_LIMIT >= 20, "{}", PANIC_REPORT_LIMIT);
    }

    /// Why the depth may not be cleared on its own, which is the trap the
    /// first version of this guard fell into: with this CPU's bit still set, a
    /// cleared depth makes the very next entry read `Report`. A fault inside
    /// containment would then run the whole banner and backtrace again, over
    /// the report the guard exists to keep.
    #[test]
    fn clearing_the_depth_alone_would_hand_a_re_entry_the_full_report() {
        assert_eq!(panic_entry(0, true), PanicEntry::Report);
    }

    /// So both are cleared together, and only at the point after which nothing
    /// of the contained panic runs. A fault the kernel survived must not reduce
    /// the next genuine panic -- about something else entirely -- to one line.
    #[test]
    fn releasing_the_guard_disarms_the_depth_and_the_bitmap_together() {
        use core::sync::atomic::Ordering;
        let (saved_d, saved_c) = (
            PANIC_DEPTH.load(Ordering::SeqCst),
            PANICKING_CPUS.load(Ordering::SeqCst),
        );
        PANIC_DEPTH.store(3, Ordering::SeqCst);
        PANICKING_CPUS.store(1 << panicking_cpu(), Ordering::SeqCst);
        release_panic_guard();
        assert_eq!(PANIC_DEPTH.load(Ordering::SeqCst), 0);
        assert_eq!(
            PANICKING_CPUS.load(Ordering::SeqCst) & (1 << panicking_cpu()),
            0,
            "the bit outlived the depth, which is the trap above"
        );
        PANIC_DEPTH.store(saved_d, Ordering::SeqCst);
        PANICKING_CPUS.store(saved_c, Ordering::SeqCst);
    }

    /// The bitmap is keyed off the APIC-backed resolver, not GS, so that a
    /// publisher that is lying about which CPU this is cannot set a peer's bit
    /// and turn a same-CPU re-entry into a "peer panic". It must land inside
    /// the bitmap for a CPU the kernel actually runs on.
    #[test]
    fn the_reporting_cpu_is_one_the_bitmap_can_hold() {
        assert!(panicking_cpu() < 64 || panicking_cpu() == lock::cpuid::NO_CPU as usize);
    }

    // ── The panic header ────────────────────────────────────────────────────

    /// What a photograph of a halted machine has to carry. The handler writes
    /// this twice -- the stop-screen banner at the top, the reprint after the
    /// backtrace -- precisely because the first copy can be wiped off the
    /// glass, so every field has to be in the one string both of them build.
    #[test]
    fn the_panic_header_names_the_cpu_the_build_the_site_and_the_message() {
        let mut b = StackBuf::new();
        write_panic_header(
            &mut b,
            3,
            "abc1234",
            Some(("zCore/src/memory.rs", 42)),
            format_args!("assertion failed: {}", "frames.is_empty()"),
        );
        let s = b.valid_str();
        assert!(s.contains("cpu=3"), "{s}");
        assert!(s.contains("[build abc1234]"), "{s}");
        assert!(s.contains("zCore/src/memory.rs:42"), "{s}");
        assert!(s.contains("assertion failed: frames.is_empty()"), "{s}");
        assert_eq!(b.dropped(), 0);
    }

    /// `PanicInfo::location` is an `Option`, and the arm that has no location
    /// still has to carry the message: a panic from a context the compiler
    /// could not place is exactly the one nobody can guess at from the
    /// backtrace alone.
    #[test]
    fn the_panic_header_without_a_location_still_carries_build_and_message() {
        let mut b = StackBuf::new();
        write_panic_header(&mut b, 0, "abc1234", None, format_args!("out of memory"));
        let s = b.valid_str();
        assert!(s.contains("cpu=0"), "{s}");
        assert!(s.contains("[build abc1234]"), "{s}");
        assert!(s.contains("out of memory"), "{s}");
        // No stray separator left behind by the missing site.
        assert!(!s.contains("::"), "{s}");
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

    /// The symbol off the capture of 2026-10-09, verbatim as `ksyms` folds it,
    /// plus the other shape the same generic can fold to (the trait side).
    /// This is the line that decides the verdict, so the match has to survive
    /// the mangling it arrives wrapped in.
    #[test]
    fn a_holder_sitting_in_the_kernel_page_fault_handler_is_recognised() {
        assert!(sym_is_diagnosing(
            "<zcore::handler::ZcoreKernelHa..as kernel_hal::kernel_handler::KernelHandler>::handle_page_fault"
        ));
        assert!(sym_is_diagnosing(
            "<T as kernel_hal::kernel_handler::KernelHandler>::handle_page_fault"
        ));
        // And the bare type name is not enough on its own: the pairing is what
        // separates the zCore handler from a handler of somebody else's.
        assert!(!sym_is_diagnosing(
            "<SomeOtherKernelHandler>::handle_page_fault"
        ));
        assert!(sym_is_diagnosing("zcore::handler::print_fault_backtrace"));
        assert!(sym_is_diagnosing("zcore::oops::try_contain"));
        assert!(sym_is_diagnosing("rust_begin_unwind"));
    }

    /// The two false positives that would make this verdict worse than the
    /// ambiguous one it replaces, because it ends in "Not AB-BA" and a cycle
    /// is exactly what it would be hiding:
    ///
    ///  * `zircon_object`'s own page-fault handlers, where a holder is taking
    ///    a fault its vmar RESOLVES;
    ///  * `panic_banner`, which the deadlock reporter calls ITSELF, so a peer
    ///    NMI landing there is the renderer, not a fault.
    #[test]
    fn a_resolvable_vm_fault_or_the_renderer_is_not_the_fault_path() {
        for sym in [
            "zircon_object::vm::vmar::VmAddressRegion::handle_page_fault",
            "<zircon_object::hypervisor::guest::Guest>::handle_page_fault",
            "kernel_hal::console::panic_banner",
        ] {
            assert!(!sym_is_diagnosing(sym), "{} read as the fault path", sym);
        }
    }

    /// Narrow on purpose: the sites the captures actually show a holder wedged
    /// at with no fault anywhere -- the allocator's coalescing scan and a
    /// shootdown wait -- must not be read as "it faulted", or the verdict
    /// sends the reader looking for a fault report that was never printed.
    #[test]
    fn an_ordinary_wedged_holder_is_not_read_as_a_fault() {
        for sym in [
            "<zcore::memory::LockedHeap<32> as core::alloc::global::GlobalAlloc>::dealloc",
            "lock::ticket::TicketMutex<T>::lock",
            "kernel_hal::vm::flush_tlb_range",
            "hal_cpu_idle",
            "zcore::handler::handle_syscall",
        ] {
            assert!(!sym_is_diagnosing(sym), "{} read as the fault path", sym);
        }
    }

    /// The banner rasterizes over whatever is on the glass, so on the one
    /// verdict that says "diagnose the fault printed above it" the paint has
    /// to be skipped: a capture from 16-oct-2026 came back with that very
    /// fault report -- an `alloc_error` naming the size that could not be
    /// allocated -- overwritten by four copies of this banner.
    #[test]
    fn the_banner_stays_off_the_glass_when_it_is_only_a_consequence() {
        assert!(verdict_follows_a_printed_fault(false, false, true));
        assert!(verdict_follows_a_printed_fault(true, false, false));
    }

    /// Shootdown starvation is a finding in its own right: nothing was printed
    /// underneath it, and the `diagnosing` flag can be set on that run anyway
    /// (a holder answering the NMI from inside a fault path it will return
    /// from). Reading the flag alone would blank the screen on the one banner
    /// that IS the report.
    #[test]
    fn a_shootdown_verdict_is_painted_even_with_a_diagnosing_holder() {
        assert!(!verdict_follows_a_printed_fault(false, true, true));
        assert!(!verdict_follows_a_printed_fault(false, true, false));
    }

    /// The open verdict ("either AB-BA or a holder stuck inside its own
    /// critical section") is the common case and has to reach a machine with
    /// no serial.
    #[test]
    fn an_ordinary_deadlock_is_still_painted() {
        assert!(!verdict_follows_a_printed_fault(false, false, false));
    }

    /// Self-deadlock wins over the rest, as it does in the verdict chain.
    #[test]
    fn a_self_deadlock_follows_a_fault_whatever_else_is_set() {
        for shootdown in [false, true] {
            for diagnosing in [false, true] {
                assert!(
                    verdict_follows_a_printed_fault(true, shootdown, diagnosing),
                    "self-deadlock + {} + {} read as its own finding",
                    shootdown,
                    diagnosing
                );
            }
        }
    }

    /// The verdict is written after `release_reserve`, so the reserve is the
    /// only thing standing between the longest verdict and a photograph that
    /// ends mid-sentence. Longest, plus the `did not fit` line that may follow
    /// it.
    #[test]
    fn the_longest_verdict_still_fits_the_bytes_held_back_for_it() {
        let faulted_holder = format!(
            "\nDIAG: HOLDER cpu{} faulted or panicked INSIDE its critical section (it \
             is in the kernel's own fault/panic path), so that lock is never given \
             back. Not AB-BA: this report is a CONSEQUENCE of the fault printed above \
             it, not a second bug. Diagnose that fault.",
            usize::MAX
        );
        const DID_NOT_FIT: &str = "\n[1234 B of this report did not fit]";
        assert!(
            faulted_holder.len() + DID_NOT_FIT.len() <= VERDICT_RESERVE,
            "{} B of verdict and tail for a {} B reserve",
            faulted_holder.len() + DID_NOT_FIT.len(),
            VERDICT_RESERVE,
        );
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

    /// Two files, as two distinct `&'static str` pointers would be.
    const F: usize = 0x1000;
    const G: usize = 0x2000;

    /// The shape the bring-up box produced: one cpu, one line
    /// (`uart_16550.rs:249`), recorded as holder AND as waiter. The old verdict
    /// named "AB-BA or a HOLDER stuck inside its own critical section" and
    /// pointed at an "is now at" line that was a stale idle RIP, so the hunt
    /// went looking for a second lock and for a cpu that had parked itself
    /// holding one. There was neither.
    #[test]
    fn a_cpu_recorded_at_one_site_as_both_holder_and_waiter_is_waiting_on_itself() {
        let site = (F, dl_pack(249, 18));
        assert_eq!(self_deadlock_cpu(&[site], &[site]), Some(18));
    }

    /// The false positive this predicate is keyed on the whole site to avoid.
    /// In a two-lock cycle each cpu is a holder of one lock and a waiter for
    /// the other, so **matching on the cpu alone calls every AB-BA a
    /// self-deadlock** -- and silences the verdict that was right about it.
    /// The two roles are recorded at different acquire sites, which is what
    /// tells them apart.
    #[test]
    fn two_cpus_holding_what_the_other_waits_for_is_not_a_self_deadlock() {
        let holders = [(F, dl_pack(10, 1)), (G, dl_pack(20, 2))];
        let waiters = [(G, dl_pack(20, 1)), (F, dl_pack(10, 2))];
        assert_eq!(self_deadlock_cpu(&holders, &waiters), None);
    }

    /// One cpu taking two different locks in two files is the same story in
    /// miniature: both roles, one cpu, and still not a re-entry.
    #[test]
    fn one_cpu_holding_one_lock_and_waiting_for_another_is_not_a_self_deadlock() {
        let holders = [(F, dl_pack(10, 4))];
        let waiters = [(G, dl_pack(10, 4))];
        assert_eq!(self_deadlock_cpu(&holders, &waiters), None);
    }

    /// The ordinary convoy: a holder that is simply slow, with other cpus
    /// queued behind it -- at the same line, which is what a lock with one
    /// acquire site produces.
    #[test]
    fn cpus_queued_behind_a_holder_at_the_same_line_are_not_a_self_deadlock() {
        let holders = [(F, dl_pack(249, 3))];
        let waiters = [(F, dl_pack(249, 0)), (F, dl_pack(249, 1))];
        assert_eq!(self_deadlock_cpu(&holders, &waiters), None);
        assert_eq!(self_deadlock_cpu(&[], &waiters), None);
    }

    /// The verdict is written after `release_reserve`, out of a fixed budget.
    /// A verdict longer than the reserve is a verdict that gets cut on the one
    /// line a photograph is read for.
    #[test]
    fn the_self_deadlock_verdict_fits_in_the_bytes_held_back_for_it() {
        let mut b = StackBuf::with_reserve(VERDICT_RESERVE);
        let _ = write!(b, "{}", "x".repeat(BANNER_BYTES * 2));
        b.release_reserve();
        let before = b.valid_str().len();
        let _ = write!(
            b,
            "\nDIAG: self-deadlock on cpu{} -- it HOLDS the lock it is waiting \
             for. A fault or panic was taken inside that critical section and \
             the handler came back in. Not AB-BA: the re-entry is the bug.",
            63
        );
        assert!(
            b.valid_str().len() > before && b.valid_str().ends_with("the re-entry is the bug."),
            "the verdict was cut: {:?}",
            &b.valid_str()[b.valid_str().len().saturating_sub(40)..]
        );
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
        // Two HOLDERs are printed, and each may carry HOLDER_CHAIN_FRAMES
        // `called from` lines. They are croppable, but cropping them is how
        // the banner stops answering the question it was grown to answer, so
        // they are budgeted like everything else.
        let mut chain = StackBuf::new();
        let _ = write!(
            chain,
            "\n   called from {}",
            kernel_hal::ksyms::Addr(0xffff_ff00_0007_3bf2u64)
        );
        let chains = 2 * HOLDER_CHAIN_FRAMES * chain.valid_str().len();
        assert_eq!(chain.dropped(), 0, "the measurement itself was cut");
        assert!(
            MAX_NONACKER_LINES * line + slot_table + chains + VERDICT_RESERVE <= BANNER_BYTES,
            "{} non-acker lines of {line} B plus the slot table ({slot_table} B), \
             the holder chains ({chains} B) and the verdict ({VERDICT_RESERVE} B) \
             do not fit in {BANNER_BYTES} B",
            MAX_NONACKER_LINES
        );
    }

    /// The guards are the whole of [`walk_frame_chain`]: it reads another
    /// cpu's stack, and the cpu it reads is wedged, not stopped. A chain that
    /// points back at itself used to be the painter's own hang.
    #[test]
    fn a_frame_chain_is_walked_only_as_far_as_it_can_be_vouched_for() {
        const STACK: u64 = 0xffff_ff00_0010_0000;
        const TEXT: u64 = 0xffff_ff00_0007_0000;
        let stack_ok = |a: u64| a >= 0xffff_8000_0000_0000 && a % 8 == 0;
        let text_ok = |a: u64| (TEXT..TEXT + 0x1000).contains(&a);

        // Three frames, each pointing at the next one up the stack.
        let frames = |a: u64| match a {
            x if x == STACK => STACK + 0x100,
            x if x == STACK + 8 => TEXT + 0x10,
            x if x == STACK + 0x100 => STACK + 0x200,
            x if x == STACK + 0x108 => TEXT + 0x20,
            x if x == STACK + 0x200 => STACK + 0x300,
            x if x == STACK + 0x208 => TEXT + 0x30,
            _ => 0,
        };
        let mut out = [0u64; 4];
        assert_eq!(
            walk_frame_chain(STACK, frames, stack_ok, text_ok, &mut out),
            3
        );
        assert_eq!(&out[..3], &[TEXT + 0x10, TEXT + 0x20, TEXT + 0x30]);

        // The cap is the caller's slice, not the chain's length.
        let mut two = [0u64; 2];
        assert_eq!(
            walk_frame_chain(STACK, frames, stack_ok, text_ok, &mut two),
            2
        );

        // A frame pointing at itself is a loop; the frame it names is still a
        // real caller, so it is reported and the walk then stops.
        let selfloop = |a: u64| match a {
            x if x == STACK => STACK,
            x if x == STACK + 8 => TEXT + 0x10,
            _ => 0,
        };
        let mut out = [0u64; 4];
        assert_eq!(
            walk_frame_chain(STACK, selfloop, stack_ok, text_ok, &mut out),
            1,
            "un marco que se apunta a si mismo tiene que cortar el paseo"
        );

        // A return address that is not kernel text is not a caller: nothing is
        // printed rather than a number `addr2line` cannot place.
        let junk = |a: u64| {
            if a == STACK + 8 {
                0x4142
            } else {
                STACK + 0x100
            }
        };
        let mut out = [0u64; 4];
        assert_eq!(
            walk_frame_chain(STACK, junk, stack_ok, text_ok, &mut out),
            0
        );

        // No frame pointer at all (the NMI landed in ring 3, or never landed).
        let mut out = [0u64; 4];
        assert_eq!(walk_frame_chain(0, frames, stack_ok, text_ok, &mut out), 0);
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
