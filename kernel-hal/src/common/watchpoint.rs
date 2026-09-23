//! [diag] Hardware data watchpoints (x86 debug registers DR0–DR3).
//!
//! Built for one specific job: catching the wild write documented in
//! `docs/README-crash-repro.md` — the one that sprays small garbage values
//! (`0x01`, `0x0a`, `0x87`) over live kernel pointers, mangling saved return
//! addresses on the executor stack and `Arc`/vtable words on the heap.
//!
//! That hunt ruled watchpoints out, and rightly so *for its chosen target*:
//! the corrupted slot was a saved return address in `run_executor`'s frame —
//! a slot the kernel legitimately writes on every call and `ret`, so a
//! write-watch there fires constantly and cannot single out the wild write
//! (and debug registers cannot match on a *value*).
//!
//! The reasoning does not carry to a **write-once** victim. A `KObjectBase`
//! name is a `String` built when the object is created and then only read;
//! a page-fault report showing a process name as `"l\u{fffd}"` means that
//! buffer was overwritten by something that had no business touching it. A
//! write-watch on those bytes is silent under normal operation, so the first
//! trap it takes is the corruptor itself — with its RIP in the trap frame.
//! That is the datum the whole investigation has been missing.
//!
//! Mechanics, and why it is shaped this way:
//! - Debug registers are **per-CPU** and are not part of the task context, so
//!   arming DR0 on the CPU that happens to call [`watch_write`] would leave
//!   every other core unwatched — and the corruptor is as likely to run there.
//!   Instead the request is published globally with a generation counter, and
//!   each CPU programs its own registers from the timer tick ([`sync_this_cpu`],
//!   called next to the executor canary check), so the watch is live on every
//!   core within one tick (~4 ms).
//! - A data watchpoint is a **trap**, not a fault: it is delivered *after* the
//!   store retires, so the handler reports and resumes. The write still lands
//!   — the point is to name the writer, not to prevent the corruption.
//! - DR7 is programmed with the *local* enable bit only; DR6's sticky match
//!   bits are cleared on the way out, as the architecture requires (the CPU
//!   never clears them itself, and a stale bit makes the next trap
//!   unattributable).
//!
//! Costs nothing when idle: no watchpoint means DR7 stays 0 and the per-tick
//! sync is a single relaxed load and compare.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

use crate::config::MAX_CORE_NUM;

/// Watched address (0 = no watchpoint armed).
static WP_ADDR: AtomicU64 = AtomicU64::new(0);
/// Watched span in bytes: 1, 2, 4 or 8.
static WP_LEN: AtomicUsize = AtomicUsize::new(0);
/// Bumped on every arm/disarm so each CPU notices it must reprogram.
static WP_GEN: AtomicU64 = AtomicU64::new(0);
/// Generation each CPU has already written into its own debug registers.
static WP_CPU_GEN: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
/// Executor spine-slot generation each CPU last programmed (see below).
static SPINE_CPU_GEN: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
/// How many times the watchpoint has fired (reported in the trap log).
static WP_HITS: AtomicU64 = AtomicU64::new(0);

// ── Executor spine-slot auto-watch (null-exec hunt, generation 2) ────────────
//
// The executor crate publishes each live executor's *spine slot* — the
// write-once `return-into-run_executor` qword at `stack_top - 0x508` that
// every `[null-exec]` capture shows zeroed (see the registry doc in
// `PreemptiveScheduler/src/executor.rs`). Nothing may legitimately store to a
// REGISTERED slot, so this module keeps DR0–DR3 loaded with the first four
// registered slots on every CPU: the corruptor's store traps on the CPU that
// executes it, with the writer's rip in the trap frame — the datum six
// post-mortem captures could not provide. A manual [`watch_write`] keeps
// priority on DR0; spine slots fill the remaining registers.
//
// Noise filtering is structural, not heuristic: registration brackets the
// write-once window (armed only between `Executor::run` entry and its
// return/Drop), and a hit whose address is no longer registered — the stack
// was retired and re-poisoned between our last tick sync and the store — is
// dropped silently.

/// What this CPU currently holds in DR0..DR3 (0 = disabled), for `#DB`
/// attribution. Written only by the owning CPU's `sync`.
static DR_ARMED: [[AtomicU64; 4]; MAX_CORE_NUM] =
    [const { [const { AtomicU64::new(0) }; 4] }; MAX_CORE_NUM];
/// Full spine-writer reports so far; arming stops after a few so a pathological
/// hot slot cannot storm the console.
static SPINE_TRAP_HITS: AtomicU64 = AtomicU64::new(0);
/// Latched off after enough reports (checked by `sync`).
static SPINE_WATCH_OFF: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Arm a write watchpoint covering `len` bytes at `addr` on **every** CPU.
///
/// `len` must be 1, 2, 4 or 8 and `addr` must be `len`-aligned — x86 ignores
/// misaligned watchpoints rather than reporting an error, so a bad request is
/// rejected here instead of silently never firing.
///
/// Only one watchpoint (DR0) is used: the callers that matter arm one target
/// and want it live everywhere, and keeping DR1–DR3 free leaves room for a
/// second probe without reworking this.
pub fn watch_write(addr: usize, len: usize) -> bool {
    if !watchable(addr as u64, len) {
        return false;
    }
    WP_ADDR.store(addr as u64, Relaxed);
    WP_LEN.store(len, Relaxed);
    WP_GEN.fetch_add(1, Relaxed);
    true
}

/// Disarm the watchpoint on every CPU.
pub fn clear_watch() {
    WP_ADDR.store(0, Relaxed);
    WP_LEN.store(0, Relaxed);
    WP_GEN.fetch_add(1, Relaxed);
}

/// The currently armed (address, len), or None.
pub fn armed() -> Option<(usize, usize)> {
    let a = WP_ADDR.load(Relaxed) as usize;
    (a != 0).then(|| (a, WP_LEN.load(Relaxed)))
}

/// Number of times the watchpoint has fired since boot.
pub fn hits() -> u64 {
    WP_HITS.load(Relaxed)
}

// ── DR7 encoding and slot allocation ─────────────────────────────────────────
//
// Architecture arithmetic and bookkeeping, with no register access, so it
// compiles — and can be tested — on every target. It used to live inside the
// bare-x86 module below, which no build this project runs ever compiles: not
// the host test binaries, not the libos kernels, not the aarch64 or riscv64
// ones. A watchpoint that watches the wrong bytes is silent by construction —
// the tool's whole output is "nothing wrote here" — so there was no build in
// which a mistake here could show up as anything at all.

/// DR7: local exact-breakpoint reporting (ignored by modern CPUs, but the
/// architecture manual still recommends setting it).
const DR7_LE: u64 = 1 << 8;

/// DR7 local-enable bit for slot `i` (bits 0, 2, 4, 6).
fn l_bit(i: usize) -> u64 {
    1 << (i * 2)
}

/// DR7 R/W field for slot `i`: 0b01 = break on data writes only.
fn rw_write(i: usize) -> u64 {
    0b01 << (16 + i * 4)
}

/// DR7 LEN code for a watch of `len` bytes, or `None` when the hardware has no
/// encoding for that width.
///
/// The encoding is *not* ordinal: 8 bytes is 0b10, which sorts between 2 and 4
/// bytes. This used to fall through to 0b11 for anything it did not recognise,
/// so a length x86 cannot express quietly became a four-byte watch on the same
/// address — and a watch over the wrong span produces exactly the same output
/// as a watch over the right span that nothing wrote to.
fn len_code(len: usize) -> Option<u64> {
    match len {
        1 => Some(0b00),
        2 => Some(0b01),
        4 => Some(0b11),
        8 => Some(0b10),
        _ => None,
    }
}

/// Whether the hardware can actually watch `len` bytes at `addr`.
///
/// x86 does not report a misaligned or malformed watchpoint: it accepts the
/// programming and then never matches. So every request is checked here, both
/// on the way in ([`watch_write`]) and again when a slot is about to be armed
/// — the second time because the spine slots come from another crate and never
/// passed the first check. Zero is not an address: it is this module's "no
/// watchpoint" sentinel.
fn watchable(addr: u64, len: usize) -> bool {
    addr != 0 && len_code(len).is_some() && addr.is_multiple_of(len as u64)
}

/// The four `(addr, len)` pairs to load into DR0–DR3: the manual watch keeps
/// DR0, registered executor spine slots fill whatever is left, and `(0, 0)`
/// means "leave this register disabled".
///
/// A request the hardware cannot express does not take a register — it used to
/// sit in DR0 with its enable bit up, so the manual watch was both dead and in
/// the way of a spine slot that would have worked. Addresses are deduplicated
/// across all four, not just against the manual one: a stack that is retired
/// and re-registered can appear twice in the snapshot, and two registers on one
/// address buy nothing and report every store twice.
fn desired_slots(manual: (u64, usize), spine: &[usize]) -> [(u64, usize); 4] {
    fn take(out: &mut [(u64, usize); 4], next: &mut usize, addr: u64, len: usize) {
        if *next < 4 && watchable(addr, len) && !out[..*next].iter().any(|&(a, _)| a == addr) {
            out[*next] = (addr, len);
            *next += 1;
        }
    }
    let mut out = [(0u64, 0usize); 4];
    let mut next = 0usize;
    take(&mut out, &mut next, manual.0, manual.1);
    for &s in spine {
        take(&mut out, &mut next, s as u64, 8);
    }
    out
}

/// The DR7 value that arms `slots`, or 0 when none of them can be armed.
///
/// A slot the hardware cannot express is left disabled rather than narrowed to
/// something it can: better a register that watches nothing and says so than
/// one that watches four bytes nobody asked about.
fn dr7_for(slots: &[(u64, usize); 4]) -> u64 {
    let mut dr7 = 0u64;
    for (i, &(addr, len)) in slots.iter().enumerate() {
        if !watchable(addr, len) {
            continue;
        }
        if let Some(code) = len_code(len) {
            dr7 |= l_bit(i) | rw_write(i) | (code << (18 + i * 4));
        }
    }
    if dr7 == 0 {
        0
    } else {
        dr7 | DR7_LE
    }
}

/// DR6's low four bits say which of DR0–DR3 matched. They are sticky: the CPU
/// sets them and never clears them.
const DR6_MATCH: u64 = 0b1111;

/// Of the slots DR6 reports as matched, the ones this module armed.
///
/// A #DB can carry a match for a register we did not program — anything else
/// with access to the debug registers can set one, and a slot we disabled
/// leaves its sticky bit behind. Claiming those used to swallow the exception:
/// the caller was told "handled", so a real breakpoint never reached the
/// handler that owns it, and a sticky bit for a disabled slot was cleared on
/// the way out as if we had consumed it.
fn ours(matched: u64, armed: &[u64; 4]) -> u64 {
    let mut bits = 0u64;
    for (slot, &addr) in armed.iter().enumerate() {
        if addr != 0 && matched & (1 << slot) != 0 {
            bits |= 1 << slot;
        }
    }
    bits
}

// ── x86_64 bare implementation ───────────────────────────────────────────────

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod imp {
    use super::*;
    use core::arch::asm;

    /// SAFETY (all helpers): `mov` to/from a debug register is valid at
    /// CPL 0, which is where every caller runs (timer tick, trap handler).
    /// Debug registers hold no state the compiler tracks, so these are opaque
    /// side-effecting instructions to it.
    unsafe fn write_dr(i: usize, v: u64) {
        match i {
            0 => asm!("mov dr0, {}", in(reg) v, options(nostack, preserves_flags)),
            1 => asm!("mov dr1, {}", in(reg) v, options(nostack, preserves_flags)),
            2 => asm!("mov dr2, {}", in(reg) v, options(nostack, preserves_flags)),
            _ => asm!("mov dr3, {}", in(reg) v, options(nostack, preserves_flags)),
        }
    }

    unsafe fn write_dr7(v: u64) {
        asm!("mov dr7, {}", in(reg) v, options(nostack, preserves_flags));
    }

    unsafe fn read_dr6() -> u64 {
        let v: u64;
        asm!("mov {}, dr6", out(reg) v, options(nostack, preserves_flags));
        v
    }

    unsafe fn write_dr6(v: u64) {
        asm!("mov dr6, {}", in(reg) v, options(nostack, preserves_flags));
    }

    /// Bring this CPU's debug registers in line with the requested manual
    /// watchpoint (DR0 priority) plus the executor spine slots (remaining
    /// registers). Re-programs only when either generation moved.
    pub(super) fn sync(cpu: usize) {
        let want_wp = WP_GEN.load(Relaxed);
        let want_spine = if SPINE_WATCH_OFF.load(Relaxed) {
            u64::MAX // latched off: stable sentinel so we reprogram once
        } else {
            ::executor::spine_gen()
        };
        if cpu >= MAX_CORE_NUM
            || (WP_CPU_GEN[cpu].load(Relaxed) == want_wp
                && SPINE_CPU_GEN[cpu].load(Relaxed) == want_spine)
        {
            return;
        }

        // Compose the desired four (addr, len) pairs.
        let manual = (WP_ADDR.load(Relaxed), WP_LEN.load(Relaxed));
        let mut slots = [0usize; 4];
        let n = if SPINE_WATCH_OFF.load(Relaxed) {
            0
        } else {
            ::executor::spine_snapshot(&mut slots)
        };
        let desired = desired_slots(manual, &slots[..n]);
        let dr7 = dr7_for(&desired);

        // Program: disable everything first so no CPU ever runs with DR7
        // enabled against a half-written DRn, then load and enable.
        //
        // `DR_ARMED` records what this CPU is *watching*, so a slot DR7 leaves
        // disabled is recorded as 0 — otherwise a stale DR6 bit would be
        // attributed to an address no register is actually matching on.
        unsafe {
            write_dr7(0);
            for (i, &(addr, _)) in desired.iter().enumerate() {
                let armed = if dr7 & l_bit(i) != 0 { addr } else { 0 };
                write_dr(i, armed);
                DR_ARMED[cpu][i].store(armed, Relaxed);
            }
            if dr7 != 0 {
                write_dr7(dr7);
            }
        }
        WP_CPU_GEN[cpu].store(want_wp, Relaxed);
        SPINE_CPU_GEN[cpu].store(want_spine, Relaxed);
    }

    /// Frame-pointer backtrace of the writer — the caller chain names the code
    /// path, not just the storing instruction (often an inlined memcpy).
    fn report_writer_chain(tag: &str, rbp: u64) {
        let plausible = |a: u64| (0xffff_ff00_0000_0000..0xffff_ff00_1000_0000).contains(&a);
        let mut fp = rbp;
        let mut i = 0usize;
        while i < 12 && plausible(fp) && (fp & 0x7) == 0 {
            // SAFETY: fp is a validated, 8-aligned kernel-range address; a
            // frame is [saved_rbp, return_addr].
            let saved = unsafe { core::ptr::read_volatile(fp as *const u64) };
            let ret = unsafe { core::ptr::read_volatile((fp + 8) as *const u64) };
            crate::console::serial_write_fmt_spin(format_args!(
                "[{tag}]   #{i:02} ret={}\n",
                crate::ksyms::Addr(ret)
            ));
            if saved <= fp {
                break; // frame pointers must strictly increase up the stack
            }
            fp = saved;
            i += 1;
        }
    }

    /// Handle a #DB. Returns true when it was one of our watchpoints (and
    /// therefore already reported and cleared), false for any other debug
    /// exception — a real breakpoint, single-step — which the caller must
    /// handle as before.
    pub(super) fn handle_debug(rip: u64, rsp: u64, rbp: u64, cpu: usize) -> bool {
        // DR6 bits 0-3 = DRn matched. The bits are sticky: the CPU sets them
        // and never clears them, so a stale bit would misattribute the next
        // #DB. Clear the ones we consume.
        let dr6 = unsafe { read_dr6() };
        let matched = dr6 & DR6_MATCH;
        if matched == 0 {
            return false;
        }
        // Clearing the sticky bits is hygiene and belongs to whoever notices
        // them, whether or not the exception turns out to be ours: one left set
        // makes the next #DB unattributable, which is the only reason DR6 is
        // touched here at all.
        unsafe { write_dr6(dr6 & !matched) };
        if cpu >= MAX_CORE_NUM {
            return false;
        }
        let armed_here = [
            DR_ARMED[cpu][0].load(Relaxed),
            DR_ARMED[cpu][1].load(Relaxed),
            DR_ARMED[cpu][2].load(Relaxed),
            DR_ARMED[cpu][3].load(Relaxed),
        ];
        let mine = ours(matched, &armed_here);
        if mine == 0 {
            // A match for a register this CPU does not have armed: residue
            // from a slot we disarmed while its bit was set, and the #DB
            // itself was raised by something else. Saying "handled" here used
            // to drop that exception on the floor before its own handler ever
            // saw it.
            return false;
        }

        let manual = WP_ADDR.load(Relaxed);
        for (slot, &addr) in armed_here.iter().enumerate() {
            if mine & (1 << slot) == 0 {
                continue;
            }

            if manual != 0 && addr == manual {
                // ── Manual watch (legacy single-target path) ────────────────
                let n = WP_HITS.fetch_add(1, Relaxed) + 1;
                let len = WP_LEN.load(Relaxed);
                // Spin/blocking serial writer: this must survive even if the
                // machine is already in the corrupted state that motivated
                // the watch.
                // The value it was overwritten WITH is half the evidence: a
                // pointer says the writer thought it owned this memory (the
                // allocator handed the block out twice), a small integer or
                // ASCII says it was aiming somewhere else entirely.
                // SAFETY: the watched word is mapped — it is being watched.
                let new_val = unsafe { core::ptr::read_volatile(addr as *const u64) };
                crate::console::serial_write_fmt_spin(format_args!(
                    "\n[watchpoint] HIT #{n} on {len}B at {addr:#x} — now holds {new_val:#018x}\n\
                     [watchpoint] WRITTEN BY {} (cpu{cpu} rsp={rsp:#x} rbp={rbp:#x})\n",
                    crate::ksyms::Addr(rip),
                ));
                report_writer_chain("watchpoint", rbp);
                // Bounded catch: a handful of hits is enough to name the
                // writer's rip. Disarm after that so a hot word cannot storm
                // the console.
                if n >= 6 {
                    super::clear_watch();
                    crate::console::serial_write_str(
                        "[watchpoint] disarming after 6 hits — writer rip(s) captured above\n",
                    );
                }
                continue;
            }

            // ── Executor spine slot ─────────────────────────────────────────
            // A hit whose slot is no longer registered is pool-retire noise
            // (the stack was dropped and re-poisoned between our last tick
            // sync and this store): drop it silently.
            let Some(exec_id) = ::executor::spine_owner_of(addr as usize) else {
                continue;
            };
            // The trap is delivered AFTER the store retires, so the slot now
            // holds the corruptor's value.
            // SAFETY: registered slots stay mapped (executor stacks are
            // pooled, never unmapped).
            let new_val = unsafe { core::ptr::read_volatile(addr as *const u64) };
            let n = SPINE_TRAP_HITS.fetch_add(1, Relaxed) + 1;
            crate::console::serial_write_fmt_spin(format_args!(
                "\n[spine-writer] HIT #{n}: executor id={exec_id} spine slot {addr:#x} \
                 OVERWRITTEN with {new_val:#x}\n\
                 [spine-writer] writer: cpu{cpu} {} rsp={rsp:#x} rbp={rbp:#x}\n",
                crate::ksyms::Addr(rip),
            ));
            report_writer_chain("spine-writer", rbp);
            // The writer's locals name its buffer: dump a window around its
            // stack pointer (bounded, kernel-range guarded).
            let lo = rsp & !0x7;
            for k in 0..24u64 {
                let a = lo + k * 8;
                if !(0xffff_ff00_0000_0000..0xffff_ff01_0000_0000).contains(&a) {
                    break;
                }
                let v = unsafe { core::ptr::read_volatile(a as *const u64) };
                crate::console::serial_write_fmt_spin(format_args!(
                    "[spine-writer]   wrsp+{:#04x} @{a:#x} = {v:#018x}\n",
                    k * 8,
                ));
            }
            ::executor::note_heap_smash_suspected();
            // A few catches are enough; stop arming spine slots afterwards so
            // an unexpected legitimate writer cannot storm the console.
            if n >= 4 {
                SPINE_WATCH_OFF.store(true, Relaxed);
                crate::console::serial_write_str(
                    "[spine-writer] disarming spine watch after 4 hits — rip(s) captured above\n",
                );
            }
        }
        true
    }
}

/// Program this CPU's debug registers if the requested watchpoint changed.
/// Called from the timer tick, so every core picks up an arm/disarm within
/// one tick without an IPI.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn sync_this_cpu() {
    imp::sync(crate::cpu::cpu_id() as usize);
}

/// Handle a debug exception; see `imp::handle_debug`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn handle_debug_trap(rip: u64, rsp: u64, rbp: u64) -> bool {
    imp::handle_debug(rip, rsp, rbp, crate::cpu::cpu_id() as usize)
}

// ── Stubs for every other target (libos, riscv, aarch64) ─────────────────────

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub fn sync_this_cpu() {}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub fn handle_debug_trap(_rip: u64, _rsp: u64, _rbp: u64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `(L, R/W, LEN)` fields DR7 holds for slot `i`, read back out of a
    /// composed DR7 the way the CPU reads them.
    fn fields(dr7: u64, i: usize) -> (u64, u64, u64) {
        (
            (dr7 >> (i * 2)) & 0b1,
            (dr7 >> (16 + i * 4)) & 0b11,
            (dr7 >> (18 + i * 4)) & 0b11,
        )
    }

    fn slots(v: [(u64, usize); 4]) -> [(u64, usize); 4] {
        v
    }

    #[test]
    fn the_len_field_is_not_ordinal() {
        // Straight out of the SDM: 8 bytes sorts *between* 2 and 4. Getting
        // this pair the natural way round watches the wrong span, and a watch
        // over the wrong span looks exactly like a watch nothing wrote to.
        assert_eq!(len_code(1), Some(0b00));
        assert_eq!(len_code(2), Some(0b01));
        assert_eq!(len_code(8), Some(0b10));
        assert_eq!(len_code(4), Some(0b11));
    }

    #[test]
    fn a_width_the_hardware_cannot_express_has_no_code() {
        // This used to fall through to 0b11, so any unrecognised width quietly
        // became a four-byte watch on the same address.
        for len in [0usize, 3, 5, 6, 7, 9, 16, 64, usize::MAX] {
            assert_eq!(len_code(len), None, "len {}", len);
        }
    }

    #[test]
    fn a_misaligned_or_impossible_watch_is_refused_here() {
        // x86 does not reject a malformed watchpoint: it accepts the
        // programming and then never matches, so the only symptom is silence.
        assert!(watchable(0x2000, 8));
        assert!(watchable(0x2004, 4));
        assert!(watchable(0x2003, 1));
        assert!(!watchable(0x2004, 8), "8 bytes must be 8-aligned");
        assert!(!watchable(0x2002, 4), "4 bytes must be 4-aligned");
        assert!(!watchable(0x2001, 2), "2 bytes must be 2-aligned");
        assert!(!watchable(0x2000, 3), "no 3-byte watch exists");
        assert!(
            !watchable(0, 8),
            "zero is this module's no-watchpoint sentinel"
        );
        // The public entry point is the same decision, so it cannot drift.
        assert!(watch_write(0x3000, 8));
        assert!(!watch_write(0x3004, 8));
        assert!(!watch_write(0, 8));
        assert!(!watch_write(0x3000, 3));
        clear_watch();
    }

    #[test]
    fn the_four_slots_do_not_share_a_bit() {
        let all = dr7_for(&slots([(0x1000, 8), (0x2000, 4), (0x3000, 2), (0x4001, 1)]));
        for i in 0..4 {
            assert_eq!(fields(all, i).0, 1, "slot {} not locally enabled", i);
            assert_eq!(fields(all, i).1, 0b01, "slot {} is not write-only", i);
        }
        assert_eq!(fields(all, 0).2, 0b10);
        assert_eq!(fields(all, 1).2, 0b11);
        assert_eq!(fields(all, 2).2, 0b01);
        assert_eq!(fields(all, 3).2, 0b00);
        // The global-enable bits (1, 3, 5, 7) stay clear: these watches are
        // per-CPU by construction and G-bits are never cleared on a task
        // switch.
        for i in 0..4 {
            assert_eq!((all >> (i * 2 + 1)) & 1, 0, "slot {} set its G bit", i);
        }
        // LE is bit 8; bit 9 is GE, which is a different thing entirely.
        assert_eq!(DR7_LE, 1 << 8);
        assert_eq!(all & (1 << 8), 1 << 8, "exact-breakpoint reporting not set");
        assert_eq!(all & (1 << 9), 0, "GE set: these watches are per-CPU");
    }

    #[test]
    fn arming_one_slot_leaves_the_other_three_alone() {
        let dr7 = dr7_for(&slots([(0, 0), (0x2000, 8), (0, 0), (0, 0)]));
        assert_eq!(fields(dr7, 1), (1, 0b01, 0b10));
        for i in [0usize, 2, 3] {
            assert_eq!(fields(dr7, i), (0, 0, 0), "slot {}", i);
        }
    }

    #[test]
    fn nothing_to_arm_means_a_dr7_of_zero() {
        // Not `DR7_LE` on its own: writing a DR7 whose only set bit is LE arms
        // no breakpoint but is still a write of a non-zero DR7, and the caller
        // uses `dr7 == 0` to mean "leave the registers disabled".
        assert_eq!(dr7_for(&slots([(0, 0); 4])), 0);
    }

    #[test]
    fn a_slot_the_hardware_cannot_express_stays_disabled() {
        // Rather than being narrowed to the four bytes it does understand.
        let dr7 = dr7_for(&slots([(0x1000, 3), (0x2004, 8), (0x1000, 0), (0x4000, 8)]));
        assert_eq!(fields(dr7, 0), (0, 0, 0), "3-byte watch was armed anyway");
        assert_eq!(
            fields(dr7, 1),
            (0, 0, 0),
            "misaligned 8-byte watch was armed"
        );
        assert_eq!(fields(dr7, 2), (0, 0, 0), "zero-length watch was armed");
        assert_eq!(fields(dr7, 3), (1, 0b01, 0b10));
    }

    #[test]
    fn the_manual_watch_keeps_the_first_register() {
        let d = desired_slots((0xdead_0000, 2), &[0x1000, 0x2000, 0x3000, 0x4000]);
        assert_eq!(d[0], (0xdead_0000, 2));
        assert_eq!(d[1], (0x1000, 8));
        assert_eq!(d[2], (0x2000, 8));
        assert_eq!(d[3], (0x3000, 8));
        // Only four registers exist; the fifth candidate is dropped.
        assert!(!d.iter().any(|&(a, _)| a == 0x4000));
    }

    #[test]
    fn spine_slots_are_watched_a_whole_word_wide() {
        // The spine slot is a write-once qword, and the corruptor may store
        // any part of it.
        let d = desired_slots((0, 0), &[0x1000]);
        assert_eq!(d[0], (0x1000, 8));
    }

    #[test]
    fn a_manual_watch_the_hardware_cannot_express_does_not_take_a_register() {
        // It used to sit in DR0 with its enable bit up: dead, and in the way of
        // a spine slot that would have worked.
        let d = desired_slots((0x1004, 8), &[0x1000, 0x2000, 0x3000, 0x4000]);
        assert_eq!(d[0], (0x1000, 8));
        assert_eq!(d[3], (0x4000, 8));
        assert!(!d.iter().any(|&(a, _)| a == 0x1004));
    }

    #[test]
    fn one_address_never_takes_two_registers() {
        // A stack retired and re-registered can appear twice in the snapshot,
        // and the manual watch can name a spine slot. Two registers on one
        // address buy nothing and report every store twice.
        let d = desired_slots((0x1000, 8), &[0x1000, 0x2000, 0x2000, 0x3000]);
        assert_eq!(d[0], (0x1000, 8));
        assert_eq!(d[1], (0x2000, 8));
        assert_eq!(d[2], (0x3000, 8));
        assert_eq!(d[3], (0, 0));
    }

    #[test]
    fn a_zero_in_the_snapshot_is_an_empty_registration_not_an_address() {
        let d = desired_slots((0, 0), &[0, 0x2000, 0]);
        assert_eq!(d[0], (0x2000, 8));
        assert_eq!(d[1], (0, 0));
    }

    #[test]
    fn a_debug_trap_on_a_register_we_did_not_arm_is_not_ours() {
        // Anything else with access to the debug registers can set one, and a
        // slot we have since disabled leaves its sticky DR6 bit behind.
        // Claiming those swallowed the exception: the caller was told
        // "handled", so the handler that owns it never ran.
        let armed = [0x1000u64, 0, 0, 0];
        assert_eq!(ours(0b0010, &armed), 0);
        assert_eq!(ours(0b1100, &armed), 0);
        assert_eq!(ours(0b0001, &armed), 0b0001);
    }

    #[test]
    fn only_the_slots_we_armed_are_claimed_out_of_a_shared_trap() {
        // One #DB can report several matches at once.
        let armed = [0x1000u64, 0, 0x3000, 0];
        assert_eq!(ours(0b1111, &armed), 0b0101);
        assert_eq!(ours(0b0100, &armed), 0b0100);
        assert_eq!(ours(0, &armed), 0);
        // Only DR0..DR3 are ours to read; DR6 carries other state above bit 3.
        assert_eq!(DR6_MATCH, 0b1111);
    }

    /// The only test that touches this module's globals, deliberately: they
    /// are one set for the whole binary, and a second test racing this one
    /// would make both flaky here (where tests run in parallel) while staying
    /// green in CI (which runs them one at a time).
    #[test]
    fn arming_and_clearing_are_published_with_a_new_generation() {
        let gen0 = WP_GEN.load(Relaxed);
        // Two bytes, not the eight a spine slot uses: the width has to travel
        // with the address, or every manual watch silently becomes a qword.
        assert!(watch_write(0x4_0000, 2));
        assert_eq!(armed(), Some((0x4_0000, 2)));
        let gen1 = WP_GEN.load(Relaxed);
        assert_ne!(gen1, gen0, "no CPU would notice the new watch");

        // A refused request changes nothing, generation included: a CPU that
        // reprogrammed for it would disarm the watch that is working.
        assert!(!watch_write(0x4_0001, 2));
        assert_eq!(armed(), Some((0x4_0000, 2)));
        assert_eq!(WP_GEN.load(Relaxed), gen1);

        clear_watch();
        assert_eq!(armed(), None);
        assert_ne!(WP_GEN.load(Relaxed), gen1, "no CPU would notice the disarm");
    }
}
