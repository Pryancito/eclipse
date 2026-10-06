use kernel_hal::fault_diag::{DiagTurn, FaultDiagLatch};
use kernel_hal::{KernelHandler, MMUFlags};
use zircon_object::object::KernelObject;
use zircon_object::task::Thread;

use super::memory;

/// Prints which CPU has this address published as a live deadline, or nothing.
///
/// Only the hit is worth a clause. A miss means the word is not one of the
/// tick's published deadlines, which is already what the reader assumes of a
/// faulting address; saying "no match" would invite them to think a match was
/// on the table for every pointer that faults.
struct LiveDeadline(Option<usize>);

impl core::fmt::Display for LiveDeadline {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(cpu) => write!(
                f,
                ", AND the deadline CPU{} has published right now, so this is \
                 the tick's own stray-sweep residue and not a writer",
                cpu
            ),
            None => Ok(()),
        }
    }
}

pub struct ZcoreKernelHandler;

/// One line naming any logical cpu id that GS reported and SMP bring-up never
/// registered (see `lock::bogus_cpu_id_events`). Silent when there were none,
/// which is the expected state; allocation- and lock-free either way, so it is
/// safe from the fault path.
fn report_bogus_cpu_id_events() {
    let (last, count) = lock::bogus_cpu_id_events();
    if count == 0 {
        return;
    }
    kernel_hal::console::serial_write_fmt_spin(format_args!(
        "[cpuid-bogus] GS reported logical cpu {} ({} time(s)) naming no CPU that \
         SMP bring-up registered — this CPU ran with a GS that lied about who it \
         is, so push_off/pop_off nested on a foreign per-CPU slot. Suspect every \
         IRQ-off critical section in that window.\n",
        last, count,
    ));
}

/// Set while this machine is already diagnosing a kernel #PF (null-range OR an
/// address the user vmar cannot resolve). A second fault during
/// `format_args!` / a console write / `panic!` (heap or vtable already
/// smashed, or the graphic framebuffer torn down mid-redraw) must NOT recurse
/// into another formatted dump -- that was the infinite `[KERNEL PAGE FAULT]`
/// cascade after the first null fn-ptr, especially under DRM redraw storms
/// (QEMU resize). Both the null-range path and `report_unresolved_kernel_fault`
/// go through it, so a re-fault in EITHER diagnosis halts with a single
/// literal serial line instead of scrolling for ever.
///
/// Telling a re-entry from a peer, and why the two need opposite answers, is
/// `kernel_hal::fault_diag`.
static FAULT_DIAG: FaultDiagLatch = FaultDiagLatch::new();

/// How long this CPU waits for a peer's diagnosis before reporting anyway.
const PEER_DIAG_BUDGET: core::time::Duration = core::time::Duration::from_secs(2);

/// Take the diagnosis latch on behalf of this CPU.
fn take_fault_diag_latch() -> DiagTurn {
    FAULT_DIAG.take(kernel_hal::cpu::cpu_id() as usize)
}

/// Release the latch taken by [`take_fault_diag_latch`].
fn release_fault_diag_latch() {
    FAULT_DIAG.release();
}

/// Wait for a peer CPU's diagnosis to finish, then report anyway.
fn wait_for_peer_fault_diag() {
    FAULT_DIAG.wait_for_peer(
        PEER_DIAG_BUDGET,
        kernel_hal::timer::timer_now,
        // Lock-free, allocation-free queue work: this runs from a fault with
        // interrupts off, and two seconds is a very long time to be deaf to a
        // TLB-shootdown IPI.
        lock::pump,
    );
}

/// Write a literal to the serial console, waiting for the lock rather than
/// dropping the line.
///
/// `serial_write_str` is a single `try_lock`: on a busy console it discards
/// the message, which is how the freeze above left no trace at all. Every
/// caller here is about to halt a CPU, so its one line is the entire record
/// and must not be thrown away for a lock that is held for microseconds.
fn serial_literal_spin(s: &str) {
    kernel_hal::console::serial_write_fmt_spin(format_args!("{}", s));
}

impl KernelHandler for ZcoreKernelHandler {
    fn frame_alloc(&self) -> Option<usize> {
        memory::frame_alloc(1, 0)
    }

    fn frame_alloc_contiguous(&self, frame_count: usize, align_log2: usize) -> Option<usize> {
        memory::frame_alloc(frame_count, align_log2)
    }

    fn frame_dealloc(&self, paddr: usize) {
        memory::frame_dealloc(paddr)
    }

    fn handle_page_fault(&self, fault_vaddr: usize, access_flags: MMUFlags) {
        // Unmapped coroutine stack guard: overflow hit the hard guard instead of
        // smashing heap into a later null fn-ptr (`rip=0x3`). Report and halt —
        // do not attempt VMAR resolution.
        //
        // Deliberately neither `panic!` nor `oops::try_contain`, unlike every
        // other fault below. Both would run a lot of code — formatting through
        // the panic hook, the frame walk, `Process::exit` — on a stack that by
        // definition has just run out: the probe that faulted was the *next*
        // page down, so what is left below RSP is only whatever this frame had
        // not yet claimed. Growing into the guard again while RSP is already
        // inside it means the CPU cannot even push the fault frame, which is a
        // double fault (`#PF` has no IST here, unlike `#DF` and `#GP`).
        // Containing an overflow properly needs the fault handler to run on its
        // own stack first; until then this is a clean, diagnosable halt rather
        // than a gamble.
        #[cfg(not(feature = "libos"))]
        if kernel_hal::stack_guard::is_guard_fault(fault_vaddr) {
            let rip = kernel_hal::kstats::last_fault_rip();
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "\n[stack-guard] COROUTINE STACK OVERFLOW: fault_vaddr={:#x} \
                 access={:?} fault_rip={:#x} — growth hit an unmapped \
                 bottom/top guard around the executor stack (prevented heap smash) \
                 — halting\n",
                fault_vaddr, access_flags, rip
            ));
            kernel_hal::console::graphic_console_write_fmt_spin(format_args!(
                "\n[stack-guard] COROUTINE STACK OVERFLOW @ {:#x} rip={:#x} — halting\n",
                fault_vaddr, rip
            ));
            loop {
                core::hint::spin_loop();
            }
        }
        // Freed-stack quarantine hit (STACKQUARANTINE=1): a WRITE landed in a
        // coroutine stack that was freed and is being held write-protected. That
        // write is the use-after-free that smashes transient-executor stacks —
        // and here we have it AT THE WRITER'S rip, before the damage. Report the
        // writer and its call chain, then halt: this is the evidence the whole
        // hunt was missing, and it must survive intact.
        #[cfg(not(feature = "libos"))]
        if kernel_hal::stack_guard::is_quarantine_fault(fault_vaddr) {
            let rip = kernel_hal::kstats::last_fault_rip();
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "\n[stack-uaf] WRITE into a FREED (quarantined) coroutine stack: \
                 target={:#x} access={:?} writer_rip={:#x} — THIS is the \
                 use-after-free writer: a dangling pointer scribbling on freed \
                 stack memory. Call chain:\n",
                fault_vaddr, access_flags, rip,
            ));
            print_fault_backtrace(access_flags);
            kernel_hal::console::serial_write_str(
                "[stack-uaf] symbolize writer_rip + chain: \
                 llvm-addr2line -e <zcore.elf> -fCi <rip ...>\n\
                 \n[KERNEL BUG] halting (diag rev 10 — stack-uaf writer pinned)\n",
            );
            kernel_hal::console::graphic_console_write_fmt_spin(format_args!(
                "\n[stack-uaf] freed-stack UAF writer @ rip={:#x} target={:#x} — halting\n",
                rip, fault_vaddr,
            ));
            loop {
                core::hint::spin_loop();
            }
        }
        // Guard: very low addresses (null-pointer dereference with a field offset)
        // are never valid user or kernel mappings — they indicate a use-after-free
        // or corrupted pointer somewhere. Attempting to resolve them through the
        // thread's vmar will itself fault (the vmar/process may be the freed
        // object), causing re-entrant page faults that cascade across all CPUs.
        // Catch them early and report without touching any thread/process state.
        if fault_vaddr < 0x1000 {
            // Re-entrant path: formatting the first fault already corrupted the
            // heap enough that `format_args!`/`Write` vtables are NULL. A second
            // entry must halt with a literal string only — no fmt, no panic!.
            match take_fault_diag_latch() {
                DiagTurn::Mine => {}
                DiagTurn::ReEntry => {
                    // Genuine re-entry on this CPU: the first diagnosis itself
                    // faulted, so formatting anything more would cascade.
                    serial_literal_spin(
                        "\n[KERNEL PAGE FAULT] re-entrant null-range while diagnosing — halting\n",
                    );
                    loop {
                        core::hint::spin_loop();
                    }
                }
                DiagTurn::PeerBusy => {
                    // A PEER is diagnosing. Wait for it and report too.
                    wait_for_peer_fault_diag();
                    let _ = take_fault_diag_latch();
                }
            }
            let in_timer = kernel_hal::timer::in_timer_callback();
            // ONLY the spin serial writer: `console_write_fmt` goes through the
            // graphic console trait object and was observed to #PF again
            // (null vtable) while diagnosing — the
            // "re-entrant null-range while diagnosing" loop.
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "\n[KERNEL BUG] null-range #PF vaddr={:#x} flags={:?} rip={} \
                 (kernel-context fault — not a userspace SIGSEGV; \
                 in_timer_callback={}; not retriable)\n",
                fault_vaddr,
                access_flags,
                kernel_hal::ksyms::Addr(kernel_hal::kstats::last_fault_rip()),
                in_timer,
            ));
            // Did this CPU run with a GS that lied about who it is? A bogus
            // logical id makes `push_off`/`pop_off` nest on a FOREIGN per-CPU
            // slot, so interrupts come back on inside someone's critical
            // section — which manufactures exactly the wild writes and
            // re-entrant acquires this fault is the tail end of. Printed here
            // because a crash log is the only place anyone will look for it.
            report_bogus_cpu_id_events();
            #[cfg(not(feature = "libos"))]
            report_heap_lock_held();
            // Name the interrupted thread via serial only (never graphic fmt).
            //
            // `name()` returns a String, i.e. it ALLOCATES -- and this fault may
            // well have been taken inside the allocator itself, with the heap
            // lock held by this very CPU. The ticket mutex is not reentrant, so
            // asking for a name there wedges the CPU on a lock it already owns,
            // which the detector reported as
            //   cpu=5 at memory_x86_64.rs:766 / HOLDER cpu=5 at ...:849
            // (alloc waiting, dealloc holding, one CPU) right after this very
            // line printed. So: ids only here; names belong to post-mortem
            // userspace logs, not the fault path.
            if let Some(thread) = kernel_hal::thread::get_current_thread() {
                if let Ok(thread) = thread.downcast::<Thread>() {
                    let in_timer_note = if in_timer {
                        " — KERNEL BUG in timer callback, not this process"
                    } else {
                        ""
                    };
                    kernel_hal::console::serial_write_fmt_spin(format_args!(
                        "[diag] interrupted thread (coincidental if IRQ/timer): \
                         {:?} in process koid {} (names omitted on the fault path: \
                         object names allocate Strings, and this fault may have been \
                         taken inside the allocator){}\n",
                        thread.id(),
                        thread.proc().id(),
                        in_timer_note,
                    ));
                }
            } else {
                kernel_hal::console::serial_write_str(
                    "[diag] no current thread (IRQ / early boot / private kernel)\n",
                );
            }
            // Note: historical `[rsp0]=0x13446` / `0x13486` logs were often
            // RFLAGS|RF misread as a return address (trap.S stored &rflags in
            // tf.rsp). After the fix, rsp0 is the real faulting RSP; truncated
            // .text residue here is genuine smash — do not walk that chain.
            print_fault_backtrace(access_flags);
            // Prefer a literal halt over `panic!` here: `panic!` formats through
            // the global panic handler and can #PF again on a smashed heap,
            // which only produces the re-entrant spin with less context.
            kernel_hal::console::serial_write_str(if in_timer {
                "\n[KERNEL BUG] null-range #PF inside timer path \
                 (not caused by the interrupted userspace process)\n"
            } else {
                "\n[KERNEL BUG] null-range kernel #PF \
                 (not a userspace-caused fault)\n"
            });
            // The diagnosis above is complete, so the fault no longer has to be
            // fatal to the machine: if it happened inside one task's call chain
            // (and none of `oops`'s safety conditions is violated) that task
            // dies and the rest of the system carries on. Deliberately NOT
            // `panic!` even now — the panic handler formats through the global
            // hook, which is precisely what re-faulted on a smashed heap and
            // buried this report. `try_contain` only uses the spin writer.
            // Diagnosis is printed; release the diagnose-re-entrancy latch so a
            // LATER null-range fault (after we isolate this one and keep running)
            // can diagnose too. Without this the second contained fault would hit
            // the "re-entrant null-range while diagnosing" halt above and defeat
            // the whole point of staying up. `oops::try_contain` has its own
            // per-CPU re-entrancy guard for a fault *during* isolation.
            release_fault_diag_latch();
            #[cfg(not(feature = "libos"))]
            crate::oops::try_contain("null-range kernel #PF", None);
            // If `try_contain` returned, isolation was declined (a lock was held,
            // no coroutine to abandon, or the budget is spent) — halt as before.
            // The rev tag answers "which kernel produced this paste?" from the
            // crash text alone — klog lines are invisible at LOG=warn, and two
            // hunts have already stalled on exactly that ambiguity.
            // Onto the stop screen as well, for the same reason as the
            // unresolved-vmar path below: on a box with a monitor and no
            // serial capture, a halt that printed only to serial is a silent
            // freeze. After `try_contain`, so a contained fault leaves the
            // live desktop alone.
            #[cfg(not(feature = "libos"))]
            {
                use core::fmt::Write;
                let mut b = crate::lang::StackBuf::new();
                let _ = write!(
                    b,
                    "KERNEL NULL-RANGE PAGE FAULT cpu={} vaddr={:#x} flags={:?}\nrip={}\n\
                     in_timer={} (not a userspace-caused fault)",
                    kernel_hal::cpu::cpu_id(),
                    fault_vaddr,
                    access_flags,
                    kernel_hal::ksyms::Addr(kernel_hal::kstats::last_fault_rip()),
                    in_timer,
                );
                kernel_hal::console::panic_banner(b.valid_str());
            }
            serial_literal_spin("\n[KERNEL BUG] halting (diag rev 10)\n");
            loop {
                core::hint::spin_loop();
            }
        }

        if let Some(thread) = kernel_hal::thread::get_current_thread() {
            if let Ok(thread) = thread.downcast::<Thread>() {
                // Ring-0 EXECUTE at a RIP that is not kernel `.text` is
                // corrupt control flow. Demand-paging the userspace address
                // would map a page and then run it in kernel mode.
                let kernel_exec_smash = access_flags.contains(MMUFlags::EXECUTE)
                    && kernel_hal::kstats::last_fault_from_kernel()
                    && kernel_hal::kaddr::kernel_exec_is_corrupt_control_flow(
                        kernel_hal::kstats::last_fault_rip(),
                    );
                if !kernel_exec_smash {
                    let vmar = thread.proc().vmar();
                    if vmar.handle_page_fault(fault_vaddr, access_flags).is_ok() {
                        // Demand paging resolved it — the overwhelmingly common
                        // case. Return WITHOUT touching the diagnosis latch, so
                        // concurrent legitimate faults on other CPUs are never
                        // serialized behind it or turned into false halts.
                        return;
                    }
                }
                // Unresolved by the user vmar: a kernel-side bug — a driver
                // dereferencing an unmapped/mismapped pointer (e.g. the vendored
                // NVIDIA RM touching torn MMIO), or a corrupted fn-ptr/vtable.
                // Report on SERIAL ONLY (never the graphic console, which may be
                // the very mapping that faulted) and try to contain.
                #[cfg(not(feature = "libos"))]
                report_unresolved_kernel_fault(fault_vaddr, access_flags, true);
                #[cfg(feature = "libos")]
                panic!(
                    "handle kernel page fault error: vaddr(0x{:x}) flags({:?})",
                    fault_vaddr, access_flags
                );
            }
        } else {
            #[cfg(not(feature = "libos"))]
            report_unresolved_kernel_fault(fault_vaddr, access_flags, false);
            #[cfg(feature = "libos")]
            panic!(
                "page fault from kernel private address 0x{:x}, flags = {:?}",
                fault_vaddr, access_flags
            );
        }
    }

    fn memory_usage(&self) -> (usize, usize) {
        memory::stats()
    }

    /// The fixed kernel heap, the arena whose exhaustion ends the machine:
    ///
    ///     [PANIC] cpu=10 ... memory allocation of 24576 bytes failed
    ///       <linux_syscall::Syscall>::sys_read::{closure#0}
    ///
    /// Nothing in `/proc` carried this number, so its growth could only be
    /// observed as that crash. `/proc/meminfo` reports it now.
    fn kernel_heap_usage(&self) -> (usize, usize) {
        (memory::heap_used(), memory::heap_total())
    }

    /// `/proc/kheap`: where the heap went, by size class, live right now —
    /// the same attribution the OOM handler prints, readable before the OOM.
    /// Read it twice a few minutes apart: the class that grew is the leak.
    #[cfg(all(target_arch = "x86_64", not(feature = "libos")))]
    fn kernel_heap_report(&self) -> alloc::string::String {
        use core::fmt::Write;
        let mut s = alloc::string::String::with_capacity(1024);
        let (used, total) = (memory::heap_used(), memory::heap_total());
        let _ = writeln!(
            s,
            "KernelHeapUsed:  {:>10} kB\nKernelHeapTotal: {:>10} kB",
            used / 1024,
            total / 1024
        );
        let refused = memory::heap_reentrancy_events();
        if refused > 0 {
            let _ = writeln!(s, "ReentrancyRefusals: {}", refused);
        }
        let _ = writeln!(s, "live by size class:");
        for (i, count) in memory::heap_live_histogram().iter().enumerate() {
            if *count > 0 {
                let size = 1usize << i;
                let _ = writeln!(
                    s,
                    "  <={:>10}B x {:<8} (<= {} MiB)",
                    size,
                    count,
                    (count * size) >> 20
                );
            }
        }
        // The 4 KiB class is where ramfs file pages and memfd (wl_shm pool)
        // backing live, and those are the ones that grow with a desktop
        // session. Name them here as the OOM report does, so a reader does
        // not have to guess what 4096B x N means.
        #[cfg(feature = "linux")]
        {
            let (created, live, bytes) = linux_object::fs::memfd_stats();
            let _ = writeln!(
                s,
                "memfd: created={} live={} live_bytes={} MiB",
                created,
                live,
                bytes >> 20
            );
        }
        // WHO holds the big blocks. The size-class histogram says "7 blocks of
        // <=8 MiB"; this says which code asked for them, which is the question
        // a leak hunt actually has. Live ≥2 MiB blocks, biggest first.
        let (big, missed) = memory::heap_big_blocks();
        if big.iter().any(|(sz, _)| *sz > 0) {
            let _ = writeln!(s, "live blocks >= 2 MiB, by allocating call site:");
            for (sz, site) in big.iter() {
                if *sz == 0 {
                    continue;
                }
                let _ = write!(s, "  {:>5} KiB <-", sz >> 10);
                for f in site.iter() {
                    if *f != 0 {
                        let _ = write!(s, " {}", kernel_hal::ksyms::Addr(*f as u64));
                    }
                }
                let _ = writeln!(s);
            }
            if missed > 0 {
                let _ = writeln!(
                    s,
                    "  ({} more were not tracked: the table holds 128 live blocks)",
                    missed
                );
            }
        }
        let _ = writeln!(s, "hot exact sizes:");
        for (size, live) in memory::heap_hot_sizes() {
            if size != 0 && live > 0 {
                let _ = writeln!(s, "  {}B x {} ({} MiB)", size, live, (size * live) >> 20);
            }
        }
        s
    }

    /// See [`KernelHandler::check_user_range`]: does the current process map
    /// anything over `[vaddr, vaddr + len)`?
    ///
    /// Deliberately conservative about WHOSE range it is: with no current
    /// thread, or a thread this kernel did not make, there is no user address
    /// space to ask and the answer is `true` -- the access is not a user-pointer
    /// access at all. A false `false` would turn a working syscall into a
    /// spurious EFAULT, which is worse than the fault this is here to prevent.
    ///
    /// Once there IS a vmar, though, the question is answered properly, by
    /// [`VmAddressRegion::range_is_mapped`]. This used to check the first and
    /// last byte and nothing between, on the stated grounds that "a mapping is
    /// page-granular and contiguous, so a range that starts and ends inside one
    /// cannot have a hole" -- which is true of ONE mapping and says nothing
    /// about two. A buffer whose ends land in different mappings with a gap
    /// between them, which is the shape of every address space with libraries in
    /// it, passed. And an overflowing `len` answered `true`: an overflow in a
    /// bounds check is the one failure mode it must not have.
    fn check_user_range(&self, vaddr: usize, len: usize) -> bool {
        let Some(thread) = kernel_hal::thread::get_current_thread() else {
            return true;
        };
        let Ok(thread) = thread.downcast::<Thread>() else {
            return true;
        };
        thread.proc().vmar().range_is_mapped(vaddr, len)
    }
}

/// One line saying whether this CPU was inside the kernel heap's critical
/// section when it faulted -- printed on every kernel-side fault report,
/// because it changes what the report means and nothing said it.
///
/// Holding that lock makes the fault unrecoverable here and unreportable
/// through anything that allocates, and it wedges every other CPU that
/// reaches the allocator: the eight-second deadlock reports with a HOLDER
/// "now at handle_page_fault" are this fault, seen from the CPUs it stranded.
/// See `memory::heap_held_by_current_cpu`.
#[cfg(not(feature = "libos"))]
fn report_heap_lock_held() {
    if crate::memory::heap_held_by_current_cpu() {
        kernel_hal::oops_log::report_str(
            "[diag] THIS CPU WAS HOLDING THE KERNEL HEAP LOCK when it faulted. The fault \
             is therefore inside the allocator (its free lists are intrusive, so a wild \
             write into them faults on the next walk), it cannot be contained on this CPU, \
             and every other CPU that reaches the heap from here on spins until the \
             deadlock detector reports it eight seconds later with this CPU as HOLDER. \
             Treat any such report as a consequence of THIS fault, not a second bug.\n",
        );
    }
}

/// Report a kernel page fault the faulting thread's user vmar could not
/// resolve, then contain it (retire just the faulting coroutine) or halt
/// cleanly. Never returns.
///
/// SERIAL ONLY, and behind the `FAULT_DIAG` re-entrancy latch — this is
/// the fix for the infinite `[KERNEL PAGE FAULT]` cascade. The previous version
/// logged through `console_write_fmt`, i.e. the *graphic* console trait object.
/// When the faulting address was itself a torn framebuffer mapping — routine
/// during a DRM redraw / QEMU window resize, where `vt_console_write_str` writes
/// into `0xffff_8000_c000_0000` — that write faulted, re-entered this handler,
/// logged again, faulted again, forever, burying every crash under an endless
/// scroll. The latch turns the *second* such fault into one literal serial line
/// and a halt; the spin serial writer never depends on a live framebuffer. This
/// is the same discipline the null-range path above already uses, sharing the
/// same latch so a re-fault that crosses between the two paths is also caught.
#[cfg(not(feature = "libos"))]
fn report_unresolved_kernel_fault(
    fault_vaddr: usize,
    access_flags: MMUFlags,
    have_thread: bool,
) -> ! {
    let rip = kernel_hal::kstats::last_fault_rip();
    // A fault while we were already reporting one (classically the graphic
    // console write itself — now removed — but also any re-fault in the walk):
    // one literal line, then halt. Never recurse into another formatted dump.
    match take_fault_diag_latch() {
        DiagTurn::Mine => {}
        DiagTurn::ReEntry => {
            serial_literal_spin(
                "\n[KERNEL PAGE FAULT] re-entrant fault while diagnosing — halting\n",
            );
            loop {
                core::hint::spin_loop();
            }
        }
        DiagTurn::PeerBusy => {
            wait_for_peer_fault_diag();
            let _ = take_fault_diag_latch();
        }
    }
    // An EXECUTE fault's `vaddr` IS the branch target, so saying what shape of
    // word it is answers the first question anyone reading the report asks.
    // Without it a capture like `vaddr=0x1076f0000 flags=EXECUTE` is just a
    // number: it is a *user-half* address reached from ring 0, which is a
    // different bug from a null and from `.text` residue.
    //
    // For every access kind, not only EXECUTE. A capture came back as
    // `vaddr=0xb3ae09dd67 flags=WRITE rip=<BTreeMap<usize, PageState>::insert>`
    // with no shape named at all, and that address is a nanosecond reading of
    // this boot's monotonic clock -- a BTreeMap storing into a timestamp, which
    // is the same writer class as the EXECUTE captures and said nothing about
    // itself. A faulting address is worth classifying whichever way the access
    // went.
    let target_shape = kernel_hal::kaddr::word_shape(fault_vaddr as u64).as_str();
    // And the clock question on the faulting address too, since that is what
    // turned the EXECUTE captures from "a user-half word" into "an instant".
    let clock = kernel_hal::kaddr::clock_shape(
        fault_vaddr as u64,
        kernel_hal::deadline::duration_to_ns(kernel_hal::timer::timer_now()),
    );
    // "An absolute deadline" on its own is half an answer, and this is the half
    // that decides. `timer_tick` builds the per-CPU deadline table on its own
    // stack every tick for the stray sweep, so a deadline-shaped word can be
    // that table's residue rather than anything a writer put there. On the
    // soft-smash path, where this probe already runs, a capture settled exactly
    // that way: "AND is the deadline CPU0 has published right now, so this is
    // the tick's own stray-sweep residue and not a writer". The next capture
    // came back through *this* line instead, which named the shape and never
    // asked the question, so the same word had to be argued about again.
    let live_deadline = match clock {
        kernel_hal::kaddr::ClockShape::NotAClock => None,
        _ => kernel_hal::timer::deadline_cpu_matching(fault_vaddr as u64),
    };
    kernel_hal::oops_log::report(format_args!(
        "\n[KERNEL PAGE FAULT] vaddr={:#x}{}{} ({}{}) flags={:?} rip={} have_thread={} \
         (unresolved by the user vmar — a kernel-side bug, not a userspace \
         SIGSEGV; the text console is skipped so a torn graphic console cannot \
         re-fault us)\n",
        fault_vaddr,
        if target_shape.is_empty() { "" } else { " is " },
        target_shape,
        clock.as_str(),
        LiveDeadline(live_deadline),
        access_flags,
        kernel_hal::ksyms::Addr(rip),
        have_thread,
    ));
    report_heap_lock_held();
    print_fault_backtrace(access_flags);
    // Release the latch before containment: a successful `try_contain` retires
    // just this coroutine and resumes scheduling, so a LATER fault must be free
    // to diagnose. `try_contain` keeps its own per-CPU guard for a fault
    // *during* isolation. Deliberately not `panic!` — the panic hook formats
    // through the same global path that re-faulted on a smashed heap and buried
    // earlier reports; `try_contain` only uses the spin writer.
    release_fault_diag_latch();
    crate::oops::try_contain("kernel #PF unresolved by user vmar", None);
    // Containment declined (a lock was held, no coroutine to abandon, or the
    // budget is spent): halt with the single diagnosis above — no cascade.
    //
    // Put that diagnosis on the stop screen too, which on a machine with a
    // monitor and no serial capture is the only place anybody will ever read
    // it. The report above is serial-only, and the reasoning for that ("a torn
    // framebuffer mapping re-faults us") is about the GRAPHIC CONSOLE --
    // `vt_console_write_str`, with its cell cache and its locks.
    // `panic_banner` is the other thing entirely: raw pixel writes to the GOP
    // framebuffer behind atomics, no locks, no allocation, which is exactly why
    // the panic handler reaches for it first. Without it, a kernel #PF that
    // stranded a lock showed the owner nothing but the deadlock detector's
    // banner eight seconds later -- a convoy of CPUs stuck on a lock, and no
    // sign of the fault that stranded it.
    //
    // AFTER `try_contain`, not before: a contained fault leaves the machine
    // running, and a full-screen "the machine is halted" over a live desktop
    // would be a lie that costs the user their session.
    {
        use core::fmt::Write;
        let mut b = crate::lang::StackBuf::new();
        let _ = write!(
            b,
            "KERNEL PAGE FAULT cpu={} vaddr={:#x} flags={:?}\nrip={}\nhave_thread={} \
             (unresolved by the user vmar: a kernel-side bug, not a userspace SIGSEGV)",
            kernel_hal::cpu::cpu_id(),
            fault_vaddr,
            access_flags,
            kernel_hal::ksyms::Addr(rip),
            have_thread,
        );
        kernel_hal::console::panic_banner(b.valid_str());
    }
    serial_literal_spin("\n[KERNEL BUG] halting (diag rev 11)\n");
    loop {
        core::hint::spin_loop();
    }
}

/// [diag] Walk the frame-pointer chain from the faulting instruction and print
/// the return addresses, then fall back to a raw stack scan for kernel code
/// pointers. The wild write reproduced by `cat /proc/self/exe` faulted inside
/// compiler_builtins `set_bytes` (memset) writing to a corrupted kernel
/// destination; its own frame has no useful name, so the CALLER chain printed
/// here is what names the code that handed memset the bad pointer/length.
/// Same idea applies to an RIP that lands outside `.text` entirely (a
/// corrupted function pointer/vtable): the faulting frame itself is garbage,
/// but its caller's return address on the stack usually still is not. Uses
/// the spin/blocking serial writer so this survives even as the panic that
/// follows re-faults on a corrupted stack. rbp==0 or a scan that leaves the
/// plausible kernel range simply stops the walk.
fn print_fault_backtrace(access_flags: MMUFlags) {
    let rbp0 = kernel_hal::kstats::last_fault_rbp();
    let rsp0 = kernel_hal::kstats::last_fault_rsp();
    // Every dereference below is gated on THIS, not on address-range heuristics:
    // ask the live page table whether the address is actually readable. This is
    // what makes it safe to walk a possibly-corrupt frame chain from inside the
    // fault handler — a #PF here would re-enter the diagnosis and bury the
    // report (or double-fault: this path can run with almost no stack left).
    // `from_current` reads CR3, which in kernel-fault context is always a valid
    // table (the kernel half is shared by every address space).
    // Empty flags mean "entry present in the tables but no permissions" — that
    // is exactly how `stack_guard` takes pages away, and reading one faults
    // like any unmapped page. Hence the `!is_empty` on top of the query.
    #[cfg(all(target_arch = "x86_64", not(feature = "libos")))]
    let mapped = {
        use kernel_hal::vm::{GenericPageTable, PageTable};
        let pt = PageTable::from_current();
        move |a: u64| {
            pt.query(a as usize)
                .map(|(_, flags, _)| !flags.is_empty())
                .unwrap_or(false)
        }
    };
    #[cfg(not(all(target_arch = "x86_64", not(feature = "libos"))))]
    let mapped = |_a: u64| true;
    // Early diagnosis: after trap.S fix, rsp0 is the faulting RSP value and
    // [rsp0] is the top-of-stack qword (CALL return for null EXECUTE). A
    // truncated .text low32 with high word zero used to be blamed on stack
    // overflow — but before the fix, tf.rsp was *&rflags* and RFLAGS|RF
    // (often 0x13xxx) falsely matched the same pattern. Skip RFLAGS-shaped
    // values; with hard guards, real truncated residue means in-stack smash
    // or heap corruption, not growth past unmapped guards.
    {
        // Align with an explicit u64 mask — `rsp0` is u64; `!(0x7usize)` would
        // be a type error against it.
        let sp0 = rsp0 & !0x7u64;
        if kernel_hal::kaddr::is_kernel_stack_qword(sp0) && mapped(sp0) {
            let top = unsafe { core::ptr::read_volatile(sp0 as *const u64) };
            // `kaddr`, not a literal window. This used to ask the question
            // itself -- "low half in 64 KiB..16 MiB and not RFLAGS-shaped" --
            // against a range three times the image's real `.text` (which
            // ends around 5.7 MiB). Everything between `etext` and 16 MiB was
            // therefore read as a half-overwritten return address, and this
            // branch does not merely print: inside a timer callback it sets
            // the sticky smash flag and **halts the machine on purpose**. A
            // wrong `.text` bound here turns a recoverable fault into a hang.
            if kernel_hal::kaddr::looks_truncated_text(top) {
                // Soft-smash (NOT unmapped [stack-guard] #PF): sticky + mode proof.
                #[cfg(not(feature = "libos"))]
                {
                    executor::note_heap_smash_suspected();
                    let (hard, soft) = executor::hard_guard_executor_counts();
                    if hard > 0 {
                        kernel_hal::oops_log::report(format_args!(
                            "[diag] truncated return residue (rsp0={:#x} [rsp0]={:#x}); \
                             hard guards not hit — likely in-stack buffer smash or \
                             heap fn-ptr corruption (NOT coroutine stack overflow)\n",
                            sp0, top,
                        ));
                    } else {
                        kernel_hal::oops_log::report(format_args!(
                            "[diag] likely coroutine stack overflow — return address high \
                             word was zeroed (rsp0={:#x} [rsp0]={:#x}); the growing stack \
                             overwrote its own return-address slot with the low half of a \
                             kernel .text pointer\n",
                            sp0, top,
                        ));
                    }
                    kernel_hal::oops_log::report(format_args!(
                        "[soft-smash] hooks_registered={} hard_guard_executors={} \
                         soft_guard_executors={}\n",
                        executor::stack_guard_hooks_registered(),
                        hard,
                        soft,
                    ));
                    report_soft_smash_stack_attr(rsp0 as usize, rbp0 as usize);
                    // Do not halt inside the timer: the skip-repair already
                    // tried to resume, and isolation (without killing the
                    // interrupted pid) is strictly better than freezing.
                    if kernel_hal::timer::in_timer_callback() {
                        kernel_hal::timer::note_timer_callback_skipped();
                    }
                }
                #[cfg(feature = "libos")]
                {
                    kernel_hal::oops_log::report(format_args!(
                        "[diag] truncated return residue (rsp0={:#x} [rsp0]={:#x})\n",
                        sp0, top,
                    ));
                }
            }
        }
    }
    // Say it once when the addresses below have no names: an unpatched kernel
    // (built without `tools/gen_ksyms.py`) is indistinguishable from a
    // backtrace whose frames all fall outside the table, and only one of those
    // is worth investigating.
    if !kernel_hal::ksyms::available() {
        use core::sync::atomic::{AtomicBool, Ordering as O};
        static NOTED: AtomicBool = AtomicBool::new(false);
        if !NOTED.swap(true, O::Relaxed) {
            kernel_hal::oops_log::report_str(
                "[kfault-bt] (no in-kernel symbol table in this build — addresses \
                 are bare; symbolize with `make sym ADDRS=\"...\"` where this \
                 kernel was built)\n",
            );
        }
    }
    kernel_hal::oops_log::report(format_args!(
        "[kfault-bt] rbp={:#x} rsp={:#x} walking frames:\n",
        rbp0, rsp0,
    ));
    // Two separate bounds, deliberately different widths:
    //  - `plausible` (rbp walk): widened from the original 256 MiB to match
    //    `plausible_sp`. That bound predates the 512 MiB kernel heap, and every
    //    coroutine stack allocated out of its upper half sits ABOVE it -- a
    //    real capture (`rbp=0xffffff0021512200`, ~525 MiB in) was rejected by
    //    the very first iteration, so for those stacks this walk could not
    //    produce a single frame. Wandering off a corrupt `saved_rbp` is now
    //    prevented by something stronger than a narrow range anyway: `mapped`
    //    below asks the page table before each dereference.
    //  - `plausible_sp` (below): only ever applied to `rsp0` itself and small
    //    fixed offsets from it (the stack scan advances by 8 bytes at a time,
    //    at most 4 KiB total) -- addresses that stay local to a known-live
    //    pointer regardless of how wide this bound is.
    // `kaddr::is_kernel_addr` is that bound, written once.
    let plausible = kernel_hal::kaddr::is_kernel_addr;
    // The frame-pointer walk runs FIRST, and unconditionally.
    //
    // It used to be skipped whenever `[rsp0]` looked like smash residue -- which
    // is precisely the case where it is the only evidence left. A null call
    // through a corrupted pointer leaves `[rsp0]=0` and no usable stack scan, so
    // "skip the walk" meant every one of those crashes reported nothing at all
    // about who made the call. The re-entrant-#PF worry that motivated the skip
    // is handled properly now by `mapped`.
    let mut rbp = rbp0;
    let mut i = 0usize;
    while i < 24 && plausible(rbp) && (rbp & 0x7) == 0 && mapped(rbp) && mapped(rbp + 8) {
        // SAFETY: rbp is 8-aligned, inside the kernel range, and both words of
        // the frame were just confirmed to be on present pages -- so these
        // reads cannot fault even if the chain itself is garbage. A frame is
        // [saved_rbp, return_addr].
        let saved_rbp = unsafe { core::ptr::read_volatile(rbp as *const u64) };
        let ret = unsafe { core::ptr::read_volatile((rbp + 8) as *const u64) };
        if ret < 0x1000 {
            kernel_hal::oops_log::report(format_args!(
                "[kfault-bt]   #{:02} ret={} (rbp={:#x}) — abort walk (corrupt frame)\n",
                i,
                kernel_hal::ksyms::Addr(ret),
                rbp,
            ));
            break;
        }
        kernel_hal::oops_log::report(format_args!(
            "[kfault-bt]   #{:02} ret={} (rbp={:#x})\n",
            i,
            kernel_hal::ksyms::Addr(ret),
            rbp,
        ));
        if saved_rbp <= rbp {
            break; // frame pointers must strictly increase up the stack
        }
        rbp = saved_rbp;
        i += 1;
    }
    // Also raw-scan the stack for kernel code pointers as a fallback when the
    // frame chain is broken (memset is a leaf that may omit rbp).
    kernel_hal::oops_log::report_str("[kfault-bt] raw stack scan from rsp:\n");
    let mut sp = rsp0 & !0x7u64;
    // Wider bound for everything below: `sp` only ever advances by 8 bytes at
    // a time from `rsp0` (at most 4 KiB total across the whole scan loop), so
    // it stays local to a known-live pointer no matter how wide this bound
    // is -- unlike the rbp walk above, widening it doesn't risk chasing an
    // untrusted value far off into unmapped memory. `plausible(w)` below
    // never dereferences `w`, only reports it, so it's equally safe to use
    // here. See the top-of-function comment for why this needed widening at
    // all: two real captures showed a live rsp being rejected by the
    // original 256 MiB bound.
    let plausible_sp = kernel_hal::kaddr::is_kernel_addr;
    // [diag] The single most reliable value here, but its interpretation
    // depends on the fault type:
    //   EXECUTE fault (indirect `call` through a null/corrupted fn-ptr):
    //     The CPU pushes the return address BEFORE loading the bad RIP, so
    //     [rsp0] = the return site of the bad call (names the caller).
    //   READ/WRITE fault (null dereference in kernel data path):
    //     No push occurs; [rsp0] = the return address of the *faulting*
    //     function itself. If this is a non-kernel value (e.g. 0x13406) the
    //     return-address slot has been overwritten — likely by a coroutine
    //     stack overflow that also corrupted a heap pointer to NULL.
    // Still gated on `plausible_sp(sp)` + `mapped(sp)` (the ADDRESS, not the
    // value) though: sp itself came from the trap frame, but if the underlying
    // bug corrupts more than just one fn-ptr, rsp could be garbage too, and
    // dereferencing an unmapped address here would fault again while already
    // handling a fault -- an #PF-during-#PF is one of the CPU's own
    // double-fault triggers (see the Double Fault this exact bug produced once
    // already).
    if plausible_sp(sp) && mapped(sp) {
        let top = unsafe { core::ptr::read_volatile(sp as *const u64) };
        let label = if access_flags.contains(MMUFlags::EXECUTE) {
            "return address pushed by bad call (caller of the null fn-ptr)"
        } else {
            "return address of the faulting function (non-kernel value = stack corruption)"
        };
        kernel_hal::oops_log::report(format_args!(
            "[kfault-bt]   [rsp0]={:#x} <- {}\n",
            top, label,
        ));
    } else {
        kernel_hal::oops_log::report(format_args!(
            "[kfault-bt]   rsp0={:#x} is itself out of the plausible kernel range -- \
             not dereferencing it (avoids a #PF-during-#PF -> double fault)\n",
            sp,
        ));
    }
    let mut found = 0usize;
    let mut scanned = 0usize;
    while found < 20 && scanned < 512 && plausible_sp(sp) && mapped(sp) {
        let w = unsafe { core::ptr::read_volatile(sp as *const u64) };
        if plausible_sp(w) {
            kernel_hal::oops_log::report(format_args!("[kfault-bt]   @{:#x} = {:#x}\n", sp, w,));
            found += 1;
        }
        sp += 8;
        scanned += 1;
    }
}

/// [diag] Walk every executor (try_lock) and print whether fault rsp/rbp sit on
/// a coroutine stack, in a guard VA, or outside all stacks.
#[cfg(not(feature = "libos"))]
fn report_soft_smash_stack_attr(rsp: usize, rbp: usize) {
    let attr = executor::attribute_fault_stack_ptrs(rsp, rbp);
    let fmt_hit = |label: &str, hit: Option<executor::StackAttrHit>| match hit {
        Some(h) => kernel_hal::oops_log::report(format_args!(
            "[soft-smash] {}={:#x} -> CPU{} exec={} task={} stack_base={:#x} region={}\n",
            label,
            if label == "rsp" { rsp } else { rbp },
            h.cpu,
            h.executor_id,
            h.task_id,
            h.stack_base,
            h.region.as_str(),
        )),
        None => kernel_hal::oops_log::report(format_args!(
            "[soft-smash] {}={:#x} -> OUTSIDE all executor stacks \
                 (walked {} CPUs, skipped {}, {} executors)\n",
            label,
            if label == "rsp" { rsp } else { rbp },
            attr.cpus_walked,
            attr.cpus_skipped,
            attr.executors_seen,
        )),
    };
    fmt_hit("rsp", attr.rsp);
    fmt_hit("rbp", attr.rbp);

    // Dense .text-range scan up the victim coroutine stack. The smash zeroes
    // only the deepest 24 B (the immediate return slot), so the CALLER chain
    // above it — executor loop -> task poll -> the ioctl/driver path that led
    // here (nvidia.rs under GL=1) — is intact. The default backtrace walks the
    // rbp chain and stops at the zeroed frame; this instead sweeps every 8 B
    // qword of the used stack and prints those that fall in the kernel .text
    // range, i.e. return addresses. Symbolize them (llvm-addr2line -e zcore
    // -fCi <ret ...>) to name the function that overran its stack. Small
    // scalars, RFLAGS residue and heap/data pointers are all outside .text and
    // skipped. Best-effort and allocation-free; each read is page-table gated.
    #[cfg(all(target_arch = "x86_64", not(feature = "libos")))]
    {
        use kernel_hal::vm::{GenericPageTable, PageTable};
        // The window is the image's own `stext`..`etext`, via `kaddr`. It was
        // a pair of literals whose comment named the `etext` of one single
        // build ("~= 0x...005b_bb27"), rounded up to 6 MiB: the next build
        // that grows past that bound silently stops naming return addresses,
        // which is the one thing this scan exists to do.
        let pt = PageTable::from_current();
        let mapped = |a: u64| {
            pt.query(a as usize)
                .map(|(_, f, _)| !f.is_empty())
                .unwrap_or(false)
        };
        kernel_hal::oops_log::report_str(
            "[kchain] .text return-address chain up the victim stack \
             (symbolize: llvm-addr2line -e zcore -fCi <ret ...>):\n",
        );
        let base = (rsp as u64) & !0x7;
        let end = base.saturating_add(0x4000); // 16 KiB: covers the ioctl chain
        let (mut a, mut printed) = (base, 0u32);
        while a < end && printed < 60 {
            if mapped(a) {
                let v = unsafe { core::ptr::read_volatile(a as *const u64) };
                if kernel_hal::kaddr::is_kernel_text(v) {
                    kernel_hal::oops_log::report(format_args!(
                        "[kchain]   @{:#x} ret={}\n",
                        a,
                        kernel_hal::ksyms::Addr(v),
                    ));
                    printed += 1;
                }
            }
            a = a.saturating_add(8);
        }
        core::mem::forget(pt); // from_current() must not free the live CR3 table
        kernel_hal::oops_log::report(format_args!(
            "[kchain] end — {} .text words in [{:#x},{:#x})\n",
            printed, base, end,
        ));
    }
}

/// The guard in front of every user-pointer dereference.
///
/// `UserPtr::check_len` asks this before handing a raw pointer to a syscall,
/// and a wrong `true` is a kernel page fault with no fixup behind it -- which
/// is to say, a kernel DoS reachable from any syscall by any process. The
/// interesting cases live with the walk itself, in
/// `VmAddressRegion::range_is_mapped`; what is pinned here is the wiring.
#[cfg(test)]
mod check_user_range_tests {
    use super::*;
    use alloc::sync::Arc;
    use kernel_hal::MMUFlags;
    use zircon_object::task::{Job, Process};
    use zircon_object::vm::{VmAddressRegion, VmObject};

    const USER_RW: MMUFlags = MMUFlags::from_bits_truncate(
        MMUFlags::READ.bits() | MMUFlags::WRITE.bits() | MMUFlags::USER.bits(),
    );

    /// Run `f` inside an `async_std` task, which is where the current-thread
    /// slot lives: `kernel_hal`'s libos `set_current_thread` writes a
    /// `task_local!`, so outside a task it has nowhere to go and
    /// `get_current_thread` answers `None`.
    fn in_a_task(f: impl FnOnce()) {
        async_std::task::block_on(async move { f() });
    }

    /// A process whose address space has two mapped pages with an unmapped one
    /// between them, made the current thread's.
    fn a_process_with_a_hole_in_its_address_space() -> (Arc<VmAddressRegion>, usize) {
        let proc = Process::create(&Job::root(), "check_user_range").unwrap();
        let thread = Thread::create(&proc, "t").unwrap();
        kernel_hal::thread::set_current_thread(Some(thread));
        let vmar = proc.vmar();
        let base = vmar.addr();
        let vmo = VmObject::new_paged(1);
        vmar.map_at(0, vmo.clone(), 0, 0x1000, USER_RW).unwrap();
        vmar.map_at(0x2000, vmo, 0, 0x1000, USER_RW).unwrap();
        (vmar, base)
    }

    /// With no current thread there is no user address space to ask, so the
    /// access is not a user-pointer access and the answer is yes. A `false`
    /// here would turn every kernel-internal access into a spurious EFAULT.
    #[test]
    fn with_no_current_thread_there_is_nothing_to_refuse() {
        assert!(
            kernel_hal::thread::get_current_thread().is_none(),
            "a plain #[test] runs outside any task, which is the case under test"
        );
        assert!(ZcoreKernelHandler.check_user_range(0, 0));
        assert!(ZcoreKernelHandler.check_user_range(0xdead_beef, 0x1000));
    }

    #[test]
    fn a_range_the_process_maps_is_allowed() {
        in_a_task(|| {
            let (_vmar, base) = a_process_with_a_hole_in_its_address_space();
            assert!(ZcoreKernelHandler.check_user_range(base, 1));
            assert!(ZcoreKernelHandler.check_user_range(base, 0x1000));
        });
    }

    #[test]
    fn a_range_the_process_does_not_map_is_refused() {
        in_a_task(|| {
            let (_vmar, base) = a_process_with_a_hole_in_its_address_space();
            assert!(!ZcoreKernelHandler.check_user_range(base + 0x1000, 1));
            assert!(!ZcoreKernelHandler.check_user_range(base + 0x8000, 0x1000));
        });
    }

    /// The bug this guard had: both ends of the range are mapped and the page
    /// between them is not, and the old first-and-last-byte check said yes.
    #[test]
    fn a_range_that_spans_a_hole_is_refused() {
        in_a_task(|| {
            let (vmar, base) = a_process_with_a_hole_in_its_address_space();
            assert!(
                vmar.find_mapping(base).is_some(),
                "the first byte is mapped"
            );
            assert!(
                vmar.find_mapping(base + 0x2000).is_some(),
                "and so is the last"
            );
            assert!(
                !ZcoreKernelHandler.check_user_range(base, 0x2001),
                "a buffer spanning an unmapped page must not get a green light"
            );
        });
    }

    /// An overflowing length is refused rather than waved through: this is a
    /// bounds check, and it used to answer `true` here.
    #[test]
    fn a_length_that_wraps_the_address_space_is_refused() {
        in_a_task(|| {
            let (_vmar, base) = a_process_with_a_hole_in_its_address_space();
            assert!(!ZcoreKernelHandler.check_user_range(base, usize::MAX));
            assert!(!ZcoreKernelHandler.check_user_range(usize::MAX, 2));
        });
    }

    /// A zero-length access touches no byte, so it needs no mapping -- what a
    /// zero-length `read` or `write` relies on.
    #[test]
    fn a_zero_length_access_needs_no_mapping() {
        in_a_task(|| {
            let (_vmar, base) = a_process_with_a_hole_in_its_address_space();
            assert!(ZcoreKernelHandler.check_user_range(base + 0x1000, 0));
        });
    }
}
