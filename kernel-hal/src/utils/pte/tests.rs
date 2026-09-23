//! What a page-table entry has to do, measured on all three formats.
//!
//! [`super::super::page_table`] never sees a bit. It asks a [`GenericPTE`]
//! six questions and gives it four orders, and every answer that is wrong by
//! one bit is a page mapped somewhere else, a permission granted that was not
//! asked for, or a mapping the walker cannot find again. None of that shows
//! up on the architecture you happen to boot.
//!
//! So the same contract runs against all three here, and then each format is
//! checked bit by bit against its own manual.

use super::aarch64::AARCH64PTE;
use super::riscv64::Rv64PTE;
use super::x86_64::{self, X86PTE};
use crate::sync::Mutex;
use crate::utils::page_table::GenericPTE;
use crate::MMUFlags;

/// `pat_wc_ready` is a process-wide flag, and a test that drives both sides
/// of the branch it guards has to have it to itself: the suite runs with
/// `--test-threads=1` today, which would hide a second test taking it.
static PAT_WC: Mutex<()> = Mutex::new(());

const FRAME: usize = 0x0012_3456_7000;
const OTHER_FRAME: usize = 0x0007_6543_2000;
/// The last frame of the smallest physical address space of the three:
/// AArch64 is configured for 40 bits (`PA_1TB_BITS`), x86-64 masks 12..52 and
/// RISC-V's PPN reaches bit 56. A mask one bit short is a frame silently
/// mapped somewhere else, and only at addresses nobody tests with.
const HIGH_FRAME: usize = 0x00ff_ffff_f000;

fn rw() -> MMUFlags {
    MMUFlags::READ | MMUFlags::WRITE
}

/// A leaf entry mapping `paddr` with `flags`, built the way
/// `PageTableImpl::map` builds one.
fn a_leaf<P: GenericPTE>(mut entry: P, paddr: usize, flags: MMUFlags, huge: bool) -> P {
    entry.clear();
    entry.set_addr(paddr);
    entry.set_flags(flags, huge);
    entry
}

// ── the contract the walker depends on ─────────────────────────────────────

/// Everything `page_table.rs` assumes, for one format. Called once per
/// architecture below so a failure names the architecture.
fn the_contract<P: GenericPTE>(zeroed: fn() -> P, arch: &str) {
    // A cleared entry is nothing at all: not in use, not present, no
    // permissions, and NOT a leaf. `next_table_mut` refuses to walk through a
    // leaf, so an entry that calls itself one when it is empty turns an
    // ordinary "nothing mapped here" into "something is already mapped here".
    let mut e = zeroed();
    assert!(e.is_unused(), "{}: a cleared entry is in use", arch);
    assert!(!e.is_present(), "{}: a cleared entry is present", arch);
    assert!(
        !e.is_leaf(),
        "{}: a cleared entry calls itself a leaf",
        arch
    );
    assert!(
        e.flags().is_empty(),
        "{}: a cleared entry grants access",
        arch
    );
    assert_eq!(e.addr(), 0, "{}: a cleared entry names a frame", arch);

    // A pointer to the next table: in use, present, and not a leaf -- the
    // walker descends through exactly this.
    e.set_table(FRAME);
    assert!(!e.is_unused(), "{}: a table entry looks empty", arch);
    assert!(e.is_present(), "{}: a table entry is not present", arch);
    assert!(!e.is_leaf(), "{}: a table entry looks like a leaf", arch);
    assert_eq!(e.addr(), FRAME, "{}: a table entry lost its address", arch);

    // A 4 KiB leaf.
    let e = a_leaf(zeroed(), FRAME, rw() | MMUFlags::USER, false);
    assert_eq!(e.addr(), FRAME, "{}: a leaf lost its frame", arch);
    assert!(e.is_present(), "{}: a leaf is not present", arch);
    assert!(!e.is_unused(), "{}: a leaf looks empty", arch);
    assert_eq!(
        e.flags(),
        rw() | MMUFlags::USER,
        "{}: a leaf's permissions did not survive",
        arch
    );

    // `set_addr` REPOINTS an entry; it does not rewrite it. `update(vaddr,
    // Some(paddr), None)` -- repointing a live mapping without touching its
    // permissions -- passes no flags afterwards, so anything `set_addr`
    // drops is gone.
    let mut e = a_leaf(zeroed(), FRAME, rw() | MMUFlags::USER, false);
    e.set_addr(OTHER_FRAME);
    assert_eq!(e.addr(), OTHER_FRAME, "{}: set_addr did not take", arch);
    assert!(e.is_present(), "{}: set_addr dropped the present bit", arch);
    assert_eq!(
        e.flags(),
        rw() | MMUFlags::USER,
        "{}: set_addr dropped the permissions",
        arch
    );

    // And `set_flags` rewrites the permissions without moving the frame,
    // which is what write-protecting a page for copy-on-write does.
    e.set_flags(MMUFlags::READ | MMUFlags::USER, false);
    assert_eq!(e.addr(), OTHER_FRAME, "{}: set_flags moved the frame", arch);
    assert_eq!(
        e.flags(),
        MMUFlags::READ | MMUFlags::USER,
        "{}: set_flags did not take",
        arch
    );

    // `is_leaf` is the walker's only way to tell a mapping from a pointer to
    // the next table, and it asks at the P3 and P2 levels, where a mapping is
    // a 1 GiB or 2 MiB block. (What a 4 KiB entry answers is not part of the
    // contract and each architecture differs -- RISC-V calls every leaf a
    // leaf, x86 reads a bit that means PAT at that level -- so nothing asks.)
    assert!(
        a_leaf(zeroed(), FRAME, rw(), true).is_leaf(),
        "{}: a huge leaf does not say so",
        arch
    );

    // A frame near the top of physical memory, where a mask one bit too
    // narrow starts lying. On a machine with less RAM than that nothing ever
    // reaches these addresses, which is exactly why it would go unnoticed.
    let mut e = a_leaf(zeroed(), HIGH_FRAME, rw(), false);
    assert_eq!(
        e.addr(),
        HIGH_FRAME,
        "{}: a high frame did not survive",
        arch
    );
    e.set_addr(FRAME);
    assert_eq!(e.addr(), FRAME, "{}: could not come back down", arch);
    let mut e = zeroed();
    e.set_table(HIGH_FRAME);
    assert_eq!(
        e.addr(),
        HIGH_FRAME,
        "{}: a high table did not survive",
        arch
    );

    // Clearing gets you back to nothing, which is what `unmap` relies on.
    let mut e = a_leaf(zeroed(), FRAME, rw(), true);
    e.clear();
    assert!(e.is_unused() && !e.is_present() && !e.is_leaf(), "{}", arch);
}

/// The eight combinations of read, write, execute and user that a mapping can
/// ask for have to come back out of the entry unchanged. They are the whole
/// of what a process is allowed to do with a page, and a bit read back the
/// wrong way round is how the kernel's read-only text became
/// non-executable and its heap executable on AArch64 once.
fn permissions_round_trip<P: GenericPTE>(zeroed: fn() -> P, arch: &str) {
    let r = MMUFlags::READ;
    let w = MMUFlags::WRITE;
    let x = MMUFlags::EXECUTE;
    let u = MMUFlags::USER;
    for asked in [
        r,
        r | w,
        r | x,
        r | w | x,
        r | u,
        r | w | u,
        r | x | u,
        r | w | x | u,
    ] {
        for huge in [false, true] {
            let e = a_leaf(zeroed(), FRAME, asked, huge);
            assert_eq!(
                e.flags(),
                asked,
                "{}: {:?} came back as {:?} (huge={})",
                arch,
                asked,
                e.flags(),
                huge
            );
            assert_eq!(e.addr(), FRAME, "{}: {:?} moved the frame", arch, asked);
            assert!(e.is_present(), "{}: {:?} is not present", arch, asked);
        }
    }
}

/// A `PROT_NONE` mapping is a page the process owns and may not touch. The
/// entry has to be CLAIMED -- `map` over it must be refused -- and NOT
/// present, so the first access faults.
fn no_access_is_claimed_but_absent<P: GenericPTE>(zeroed: fn() -> P, arch: &str) {
    let e = a_leaf(zeroed(), FRAME, MMUFlags::empty(), false);
    assert!(!e.is_present(), "{}: a no-access page is present", arch);
    assert!(
        e.flags().is_empty(),
        "{}: a no-access page grants access",
        arch
    );
    assert!(!e.is_unused(), "{}: a no-access page looks unmapped", arch);
}

#[test]
fn every_format_keeps_the_walkers_contract() {
    the_contract(X86PTE::zeroed, "x86_64");
    the_contract(AARCH64PTE::zeroed, "aarch64");
    the_contract(Rv64PTE::zeroed, "riscv64");
}

#[test]
fn every_format_hands_back_the_permissions_it_was_given() {
    permissions_round_trip(X86PTE::zeroed, "x86_64");
    permissions_round_trip(AARCH64PTE::zeroed, "aarch64");
    permissions_round_trip(Rv64PTE::zeroed, "riscv64");
}

#[test]
fn a_no_access_mapping_is_claimed_but_absent() {
    no_access_is_claimed_but_absent(X86PTE::zeroed, "x86_64");
    no_access_is_claimed_but_absent(AARCH64PTE::zeroed, "aarch64");
    no_access_is_claimed_but_absent(Rv64PTE::zeroed, "riscv64");

    // Except one way, and it is worth writing down rather than leaving to be
    // rediscovered: on RISC-V a leaf descriptor is one with R, W or X set,
    // and a no-access page has none, so `is_leaf()` says no. At the 4 KiB
    // level nothing asks. At the P3 or P2 level it means a no-access 2 MiB or
    // 1 GiB mapping reads as a pointer to a table that is not there, and the
    // walker answers `NotMapped` for a range that IS mapped. Linux spends a
    // software bit (`_PAGE_PROT_NONE`, one of the two RSW bits, also spare
    // here) on exactly this.
    let e = a_leaf(Rv64PTE::zeroed(), FRAME, MMUFlags::empty(), true);
    assert!(!e.is_leaf(), "riscv64 grew a no-access block encoding");
    // The other two do carry it.
    assert!(a_leaf(X86PTE::zeroed(), FRAME, MMUFlags::empty(), true).is_leaf());
    assert!(a_leaf(AARCH64PTE::zeroed(), FRAME, MMUFlags::empty(), true).is_leaf());
}

// ── x86-64: Intel SDM vol. 3A, tables 4-15 .. 4-20 ─────────────────────────

const X86_PRESENT: u64 = 1 << 0;
const X86_WRITABLE: u64 = 1 << 1;
const X86_USER: u64 = 1 << 2;
const X86_PWT: u64 = 1 << 3;
const X86_PCD: u64 = 1 << 4;
const X86_PS: u64 = 1 << 7;
const X86_NX: u64 = 1 << 63;

#[test]
fn the_x86_entry_is_the_one_the_cpu_walks() {
    // A user page, read-write, no execute.
    let e = a_leaf(X86PTE::zeroed(), FRAME, rw() | MMUFlags::USER, false);
    assert_eq!(
        e.raw(),
        FRAME as u64 | X86_PRESENT | X86_WRITABLE | X86_USER | X86_NX
    );

    // Executable drops NX; kernel drops the user bit.
    let e = a_leaf(
        X86PTE::zeroed(),
        FRAME,
        MMUFlags::READ | MMUFlags::EXECUTE,
        false,
    );
    assert_eq!(e.raw(), FRAME as u64 | X86_PRESENT);

    // A 2 MiB leaf sets PS, which is what tells the walker to stop.
    let e = a_leaf(X86PTE::zeroed(), FRAME, MMUFlags::READ, true);
    assert_eq!(e.raw(), FRAME as u64 | X86_PRESENT | X86_PS | X86_NX);

    // No access at all is not present, whatever else was asked for: any
    // non-empty flag used to stamp PRESENT, and x86 then treated the page as
    // readable.
    let e = a_leaf(X86PTE::zeroed(), FRAME, MMUFlags::USER, false);
    assert!(!e.is_present());
    assert_eq!(e.raw() & X86_PRESENT, 0);

    // A table pointer is present, writable and user-accessible; the leaf's
    // own bits are what actually gate the access.
    let mut e = X86PTE::zeroed();
    e.set_table(FRAME);
    assert_eq!(
        e.raw(),
        FRAME as u64 | X86_PRESENT | X86_WRITABLE | X86_USER
    );
    assert_eq!(e.raw() & X86_PS, 0);
}

#[test]
fn an_uncached_x86_mapping_carries_the_cache_bits() {
    let _guard = PAT_WC.lock();
    // CACHE_1 alone is Uncached, CACHE_1|CACHE_2 is WriteCombining; both set
    // PCD|PWT, and the PAT bit is what distinguishes them.
    let uncached = a_leaf(
        X86PTE::zeroed(),
        FRAME,
        MMUFlags::READ | MMUFlags::CACHE_1,
        false,
    );
    assert_eq!(uncached.raw() & (X86_PCD | X86_PWT), X86_PCD | X86_PWT);

    // Before `pat::init_this_cpu` has redefined entry 7, PAT index 7 is still
    // uncached, so plain PCD|PWT (index 3, also uncached) is the honest
    // encoding and the PAT bit must not be emitted.
    x86_64::set_pat_wc_ready(false);
    let wc = a_leaf(
        X86PTE::zeroed(),
        FRAME,
        MMUFlags::READ | MMUFlags::CACHE_1 | MMUFlags::CACHE_2,
        false,
    );
    assert_eq!(wc.raw() & (1 << 7), 0, "the PAT bit before entry 7 is WC");

    // Once it is, a 4 KiB page takes the PAT bit at 7 and a huge leaf at 12,
    // because bit 7 in a 2 MiB or 1 GiB leaf is PS.
    x86_64::set_pat_wc_ready(true);
    let wc = MMUFlags::READ | MMUFlags::CACHE_1 | MMUFlags::CACHE_2;
    assert_eq!(
        a_leaf(X86PTE::zeroed(), FRAME, wc, false).raw() & (1 << 7),
        1 << 7
    );
    assert_eq!(
        a_leaf(X86PTE::zeroed(), FRAME, wc, true).raw() & (1 << 12),
        1 << 12
    );
    x86_64::set_pat_wc_ready(false);

    // Two traps that follow from the PAT bit moving, both unreachable today
    // and both worth naming. `addr()` masks bits 12..52, so on a huge leaf it
    // reports the PAT bit as part of the frame -- a write-combining 2 MiB
    // page answers `base + 0x1000`, which `split_huge_page` would carry into
    // every one of its 512 children. And `is_leaf()` reads bit 7, which in a
    // 4 KiB entry is the PAT bit, so a write-combining small page calls
    // itself huge. Neither happens while x86 asks for no huge mapping
    // (`MMUFlags::HUGE_PAGE` is only used by RISC-V) and while `is_leaf()` is
    // only asked at the P3 and P2 levels. Fixing either needs the entry to
    // know which level it sits at, which `GenericPTE` does not carry.
    x86_64::set_pat_wc_ready(true);
    // A 2 MiB-aligned frame, which is the only kind a 2 MiB leaf can hold,
    // and whose bit 12 is therefore clear.
    const HUGE_FRAME: usize = 0x0012_3440_0000;
    let huge_wc = a_leaf(X86PTE::zeroed(), HUGE_FRAME, wc, true);
    assert_eq!(huge_wc.addr(), HUGE_FRAME + 0x1000, "the PAT trap moved");
    let small_wc = a_leaf(X86PTE::zeroed(), FRAME, wc, false);
    assert!(small_wc.is_leaf(), "the PAT trap moved");
    x86_64::set_pat_wc_ready(false);
}

// ── AArch64: ARM ARM, stage 1 VMSAv8-64 block and page descriptors ─────────

const A64_VALID: u64 = 1 << 0;
const A64_NON_BLOCK: u64 = 1 << 1;
const A64_INNER: u64 = 1 << 8;
const A64_SHAREABLE: u64 = 1 << 9;
const A64_AP_EL0: u64 = 1 << 6;
const A64_AP_RO: u64 = 1 << 7;
const A64_AF: u64 = 1 << 10;
const A64_PXN: u64 = 1 << 53;
const A64_UXN: u64 = 1 << 54;
/// Attribute index 1 = Normal memory.
const A64_NORMAL: u64 = 1 << 2;

#[test]
fn the_aarch64_descriptor_is_the_one_the_mmu_walks() {
    let normal = A64_NORMAL | A64_INNER | A64_SHAREABLE | A64_AF;

    // A user page, read-write, no execute: EL0-accessible, and execute-never
    // at both levels.
    let e = a_leaf(AARCH64PTE::zeroed(), FRAME, rw() | MMUFlags::USER, false);
    assert_eq!(
        e.raw(),
        FRAME as u64 | normal | A64_VALID | A64_NON_BLOCK | A64_AP_EL0 | A64_PXN | A64_UXN
    );

    // A 2 MiB block clears NON_BLOCK; that bit is the whole difference
    // between a mapping and a pointer to another table.
    let e = a_leaf(AARCH64PTE::zeroed(), FRAME, rw() | MMUFlags::USER, true);
    assert_eq!(e.raw() & A64_NON_BLOCK, 0);
    assert!(e.is_leaf());

    // A kernel page that is executable: PXN clear, UXN set.
    let e = a_leaf(
        AARCH64PTE::zeroed(),
        FRAME,
        MMUFlags::READ | MMUFlags::EXECUTE,
        false,
    );
    assert_eq!(
        e.raw(),
        FRAME as u64 | normal | A64_VALID | A64_NON_BLOCK | A64_AP_RO | A64_UXN
    );
    // Read back the way the hardware reads it: PXN is Privileged eXecute
    // Never, so a kernel mapping is executable when the bit is ABSENT. This
    // was once read the other way round, which inverted EXECUTE on every
    // kernel mapping -- and `split_huge_page` round-trips flags through
    // exactly this, so the kernel's read-only text came out non-executable
    // and its heap executable.
    assert_eq!(e.flags(), MMUFlags::READ | MMUFlags::EXECUTE);

    // A table pointer: valid, NON_BLOCK, and nothing else. The next-level
    // limit bits stay clear so the leaf decides.
    let mut e = AARCH64PTE::zeroed();
    e.set_table(FRAME);
    assert_eq!(e.raw(), FRAME as u64 | A64_VALID | A64_NON_BLOCK);
}

#[test]
fn an_invalid_aarch64_descriptor_grants_nothing() {
    // Every permission bit here says what is FORBIDDEN on top of an access
    // the descriptor already allows, so none of them means anything until
    // VALID is set. Reading them out of an invalid entry is how `stack_guard`
    // lost its guard bands: a band whose flags it had just cleared still
    // reported WRITE, because AP_RO is absent from an all-zero entry exactly
    // as it is from a writable one.
    let mut e = AARCH64PTE::zeroed();
    assert!(e.flags().is_empty());
    e.set_addr(FRAME);
    assert!(e.flags().is_empty(), "an address alone grants nothing");
    // And attribute index 0 is Device, which a cleared entry has without ever
    // having been a device mapping.
    assert!(!e.flags().contains(MMUFlags::DEVICE));
}

// ── RISC-V: privileged spec, Sv39/Sv48 page-table entry ────────────────────

const RV_VALID: u64 = 1 << 0;
const RV_READABLE: u64 = 1 << 1;
const RV_WRITABLE: u64 = 1 << 2;
const RV_EXECUTABLE: u64 = 1 << 3;
const RV_USER: u64 = 1 << 4;
const RV_GLOBAL: u64 = 1 << 5;
const RV_ACCESSED: u64 = 1 << 6;
const RV_DIRTY: u64 = 1 << 7;

#[test]
fn the_riscv_entry_is_the_one_the_hardware_walks() {
    // The PPN sits at bits 10..54 and the frame is shifted right by two: the
    // entry is not the address with flags beside it, the way the other two
    // are.
    let e = a_leaf(Rv64PTE::zeroed(), FRAME, rw() | MMUFlags::USER, false);
    assert_eq!(
        e.raw(),
        (FRAME as u64 >> 2)
            | RV_VALID
            | RV_READABLE
            | RV_WRITABLE
            | RV_USER
            | RV_ACCESSED
            | RV_DIRTY
    );
    assert_eq!(e.addr(), FRAME, "the PPN did not shift back");

    // Write implies read: there is no write-only leaf encoding, and W without
    // R is a reserved combination the hardware faults on.
    let e = a_leaf(Rv64PTE::zeroed(), FRAME, MMUFlags::WRITE, false);
    assert_eq!(
        e.raw() & (RV_READABLE | RV_WRITABLE),
        RV_READABLE | RV_WRITABLE
    );

    // A leaf is one with R, W or X; a pointer to the next table has V alone.
    let mut e = Rv64PTE::zeroed();
    e.set_table(FRAME);
    assert_eq!(e.raw(), (FRAME as u64 >> 2) | RV_VALID);
    assert!(!e.is_leaf());
    assert!(a_leaf(Rv64PTE::zeroed(), FRAME, MMUFlags::READ, false).is_leaf());
    assert!(a_leaf(Rv64PTE::zeroed(), FRAME, MMUFlags::EXECUTE, false).is_leaf());

    // Accessed and dirty are set by the kernel because updating them in
    // hardware is optional in the spec; an implementation that does not
    // update them faults instead, and the fault has nowhere to go.
    assert_eq!(
        a_leaf(Rv64PTE::zeroed(), FRAME, MMUFlags::READ, false).raw() & (RV_ACCESSED | RV_DIRTY),
        RV_ACCESSED | RV_DIRTY
    );

    // The global bit is not a permission and cannot travel through
    // `set_flags`; `pt_clone_kernel_space` marks the kernel's top-level
    // entries with it so their translations survive an `satp` switch.
    let mut e = a_leaf(Rv64PTE::zeroed(), FRAME, rw(), false);
    assert_eq!(e.raw() & RV_GLOBAL, 0);
    e.set_global();
    assert_eq!(e.raw() & RV_GLOBAL, RV_GLOBAL);
    assert_eq!(e.addr(), FRAME);
    assert_eq!(e.flags(), rw(), "the global bit is not a permission");
}

/// A no-access page could once panic a debug kernel from userspace: there was
/// a `debug_assert!` in `set_flags` demanding R or X, and `mmap(PROT_NONE)`
/// -- the first thing musl's `pthread_create` does -- has neither.
#[test]
fn a_no_access_riscv_page_does_not_panic_the_kernel() {
    let e = a_leaf(Rv64PTE::zeroed(), FRAME, MMUFlags::empty(), false);
    assert_eq!(e.raw() & RV_VALID, 0);
    assert_eq!(e.addr(), FRAME);
    assert!(e.flags().is_empty());
}
