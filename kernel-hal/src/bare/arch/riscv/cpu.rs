//! CPU information.
use crate::common::cpu_topology::CpuTopology;
use crate::utils::init_once::InitOnce;

/// Tick rate of the `time` CSR, in hertz, from the device tree's
/// `timebase-frequency`. Default: QEMU `virt`'s 10 MHz.
///
/// In hertz and not megahertz because that is what the device tree says and
/// what the clock arithmetic needs. It used to be rounded to whole megahertz
/// on the way in, which is lossy for most real boards (a StarFive JH7100's
/// 6.25 MHz became 6) and zero for any board whose timebase is under a
/// megahertz — and zero is what `timer_now` divided by.
pub(super) static TIMEBASE_HZ: InitOnce<u64> = InitOnce::new_with_default(10_000_000);

/// The `time` CSR's tick rate in hertz.
pub(super) fn timebase_hz() -> u64 {
    *TIMEBASE_HZ
}

/// `VLENB` of the harts, in bytes, or zero when user mode is not given the
/// vector unit: the device tree names no `v`, or the registers are wider
/// than the `UserContext` can hold. Set once by `primary_init_early`.
static VECTOR_VLENB: InitOnce<usize> = InitOnce::new_with_default(0);

/// The width of one vector register in bytes, or zero when user mode runs
/// with `sstatus.VS` Off. See [`trapframe::VectorState`].
pub fn vector_vlenb() -> usize {
    *VECTOR_VLENB
}

/// Decide from the device tree whether user mode gets the vector unit, and
/// remember its `VLENB`.
///
/// Every enabled hart has to advertise V: a user context migrates between
/// harts, so the unit is switched on for all of them or for none. Only the
/// boot hart is asked for `vlenb`, and only once its own node has said it has
/// V: on a hart without it the CSR does not exist and the read is an illegal
/// instruction in supervisor mode, which the kernel cannot survive. Each
/// secondary checks its own `vlenb` against this one as it comes up
/// ([`check_vector_on_this_hart`]). A unit wider than
/// [`trapframe::VLENB_MAX`] stays Off, as it was before this existed, and
/// says so.
pub(super) fn init_vector(harts: &[zcore_drivers::utils::devicetree::RiscvHart<'_>]) {
    use zcore_drivers::utils::devicetree::isa_has_vector;
    if harts.is_empty() {
        info!("riscv: no harts in the device tree, vector unit stays Off for user mode");
        return;
    }
    let boot = raw_hart_id() as u64;
    let Some(this) = harts.iter().find(|hart| hart.hart_id == Some(boot)) else {
        info!("riscv: hart {boot} is not in the device tree, vector unit stays Off for user mode");
        return;
    };
    for hart in harts {
        match hart.isa {
            Some(isa) if isa_has_vector(isa) => {}
            Some(isa) => {
                info!(
                    "riscv: hart {:?} `{isa}` has no V, vector unit stays Off for user mode",
                    hart.hart_id
                );
                return;
            }
            None => {
                info!(
                    "riscv: hart {:?} has no `riscv,isa`, vector unit stays Off for user mode",
                    hart.hart_id
                );
                return;
            }
        }
    }
    // SAFETY: the device tree just said this hart (`this`, found by its id)
    // implements V.
    let _ = this;
    let vlenb = unsafe { trapframe::vlenb() };
    if vlenb == 0 || vlenb > trapframe::VLENB_MAX {
        warn!(
            "riscv: VLENB = {vlenb} bytes is more than the {} the user context holds, \
             vector unit stays Off for user mode",
            trapframe::VLENB_MAX
        );
        return;
    }
    info!(
        "riscv: vector unit on for user mode, VLENB = {vlenb} bytes (VLEN = {} bits), {} harts",
        vlenb * 8,
        harts.len()
    );
    VECTOR_VLENB.init_once_by(vlenb);
}

/// The cache block size `cbo.flush` works in, in bytes, or zero when the
/// harts have no Zicbom. Set once by `primary_init_early`.
static CBOM_BLOCK_SIZE: InitOnce<usize> = InitOnce::new_with_default(0);

/// Bytes one `cbo.*` instruction covers, or zero when the harts do not
/// implement Zicbom and `frame_flush` has nothing to walk the cache with.
pub(super) fn cbom_block_size() -> usize {
    *CBOM_BLOCK_SIZE
}

/// Decide from the device tree whether `cbo.flush` exists and how wide a
/// block is, as Linux does with `riscv,cbom-block-size` and the `zicbom`
/// of `riscv,isa` (`riscv_init_cbo_blocksize`). Without Zicbom RISC-V has
/// no cache-maintenance instruction at all, and the kernel keeps what it
/// did until now: a `fence`. Every enabled hart has to agree, since the
/// frame being flushed may be dirty in any of their caches.
pub(super) fn init_cbom(harts: &[zcore_drivers::utils::devicetree::RiscvHart<'_>]) {
    use zcore_drivers::utils::devicetree::isa_has_zicbom;
    let mut size = None;
    for hart in harts {
        let has = hart.isa.is_some_and(isa_has_zicbom);
        let block = hart.cbom_block_size.map(|b| b as usize);
        match (has, block) {
            (true, Some(block)) if block.is_power_of_two() && block >= 4 => {
                if size.is_some_and(|s| s != block) {
                    warn!(
                        "riscv: hart {:?} has a {block}-byte cache block, others {:?}: \
                         `frame_flush` falls back to a fence",
                        hart.hart_id, size
                    );
                    return;
                }
                size = Some(block);
            }
            _ => {
                info!(
                    "riscv: hart {:?} has no usable Zicbom (block {block:?}): \
                     `frame_flush` is a fence",
                    hart.hart_id
                );
                return;
            }
        }
    }
    if let Some(block) = size {
        info!("riscv: Zicbom on, `frame_flush` walks {block}-byte cache blocks");
        CBOM_BLOCK_SIZE.init_once_by(block);
    }
}

/// A secondary hart's own `vlenb` against the boot hart's.
///
/// Called from `secondary_init`, after the primary has decided. The device
/// tree vouched that this hart has V (every enabled hart had to), so the read
/// is safe; a width that differs is a machine this kernel cannot run user
/// vector code on correctly -- a state saved on one hart would be put back
/// truncated or padded on the other -- so it stops here, at boot, with the
/// reason, rather than corrupting a register file later.
pub(super) fn check_vector_on_this_hart() {
    let expected = vector_vlenb();
    if expected == 0 {
        return;
    }
    // SAFETY: `init_vector` only recorded a width after every enabled hart's
    // node advertised V.
    let vlenb = unsafe { trapframe::vlenb() };
    assert!(
        vlenb == expected,
        "riscv: hart {} has VLENB = {vlenb} bytes but the boot hart has {expected}: \
         harts with different vector widths are not supported",
        raw_hart_id()
    );
}

// ─── CPU topology: dense logical id  <->  hart id ───────────────────────────────
//
// Hart ids (in `tp`) may be sparse (some boards reserve hart 0), so they cannot be
// used directly to index per-CPU arrays. Each online hart gets a dense logical id
// (0..NCPU, boot hart = 0). The forward map (hart -> logical) lives in `lock` so
// the lock crate and the kernel share one id space; here we keep the reverse map
// (logical -> hart) needed to target SBI IPIs.

/// This machine's dense logical id <-> hart id map. Shared with x86_64 and
/// aarch64 so the three cannot drift apart again: this side used to count in a
/// `u8` with no bound, and to answer "which hart is logical N?" with hart 0 —
/// the boot hart — for an N nobody had ever registered.
static TOPOLOGY: CpuTopology = CpuTopology::new();

/// Raw hart id of the current CPU (kernel convention: stored in `tp`).
///
/// Use this — not [`cpu_id`](crate::cpu::cpu_id) — whenever a *hardware* hart id
/// is required (device-tree `riscv-intc-cpuN` nodes, PLIC contexts, SBI hart masks).
pub fn raw_hart_id() -> usize {
    let hart_id: usize;
    unsafe { core::arch::asm!("mv {0}, tp", out(reg) hart_id) };
    hart_id
}

/// Assign this hart its dense logical id and register the hart<->logical maps.
/// Called once per hart from `percpu::register`, before any lock-taking code.
pub fn register_logical_id() -> u8 {
    let hart_id = raw_hart_id() as u32;
    // A hart past what the per-CPU tables hold keeps the id it booted with
    // rather than being given one that indexes nothing: the count used to be
    // an `AtomicU8` incremented unconditionally, so hart 64 got id 64, no
    // reverse-map entry (the `.get()` dropped it silently), and a `lock`
    // forward-map entry pointing at a per-CPU slot it shares with somebody
    // else. There is nothing sound to do for such a hart here, but reporting
    // it is better than letting it corrupt another CPU's state in silence.
    let Some(logical) = TOPOLOGY.register(hart_id) else {
        warn!(
            "[smp] hart {} has no logical id left (max {}) — it must not run kernel code",
            hart_id,
            crate::config::MAX_CORE_NUM
        );
        return u8::MAX;
    };
    lock::set_logical_cpu_id(hart_id, logical as u8);
    logical as u8
}

/// Translate a dense logical CPU id back to its hart id (for SBI IPI delivery),
/// or `None` when no hart was ever given that id.
///
/// `None` rather than a fallback, for the reason x86_64's `logical_to_apic`
/// gives: hart 0 is the boot hart, so answering "unknown" with 0 does not drop
/// the IPI — it rings the BSP, which flushes a TLB nobody asked it about, while
/// the CPU the shootdown was for hears nothing and its initiator waits for an
/// acknowledgement in a loop that has no timeout.
pub fn logical_to_hart(logical: usize) -> Option<usize> {
    TOPOLOGY.hw_id(logical).map(|h| h as usize)
}

hal_fn_impl! {
    impl mod crate::hal_fn::cpu {
        fn cpu_id() -> u8 {
            // Dense logical id (0..NCPU), resolved from the sparse hart id via the
            // table in `lock`. Hart ids are not contiguous on all boards, so they
            // must not be used directly to index per-CPU arrays.
            lock::current_cpu_id()
        }

        fn cpu_frequency() -> u16 {
            // riscv gives the kernel no way to read the core clock, so what
            // `/proc/cpuinfo` has always shown here is the timebase. Kept as
            // it was rather than quietly changing what userspace reads; the
            // clock arithmetic no longer goes through this number.
            (timebase_hz() / 1_000_000) as u16
        }

        fn cpu_brand() -> alloc::string::String {
            alloc::string::String::from("RISC-V CPU")
        }

        fn cpu_count() -> u8 {
            TOPOLOGY.count() as u8
        }

        fn reset() -> ! {
            info!("shutdown...");
            sbi_rt::system_reset(sbi_rt::Shutdown, sbi_rt::NoReason);
            unreachable!()
        }
    }
}
