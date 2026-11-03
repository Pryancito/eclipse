//! The top of a 32-bit process's address space, asked of the **process**.
//!
//! Every call that hands a program an address --- `mmap`, `mremap`, `brk`,
//! `shmat` --- could place it anywhere in a 47-bit space, and an i386 program
//! gets that address back in a 32-bit register. Eclipse did exactly that: an
//! i386 program asking for two pages was handed `0xfffff000`, a mapping whose
//! second page is above 4 GiB and whose address is in the errno band every
//! libc reads as a failure. Step 56 of `tools/i386-hello` is the probe that
//! caught it.
//!
//! What the ceiling itself does is pinned next to it, in
//! `zircon_object::vm::vmar::ceiling_tests`. What is pinned here is that each
//! of these calls passes it --- the wiring, which is the part a refactor
//! drops --- and that a native process's placement is not changed by any of
//! it. The tests are deliberately paired for that reason: the same request,
//! once from each ABI.
//!
//! Fixed pids live in 47_47x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::loader::{heap_base_for, TASK_SIZE_32};
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const PAGE: usize = PAGE_SIZE;
const ANON_PRIVATE: usize = 0x22; // MAP_PRIVATE | MAP_ANONYMOUS
const ANON_PRIVATE_FIXED: usize = 0x32; // ... | MAP_FIXED
const RW: usize = 0x3;
const NO_FD: usize = -1i32 as usize;
/// `IPC_CREAT` and a mode, for the `shmat` test's own segments.
const IPC_CREAT: usize = 0o1000;
const EVERYBODY: usize = 0o666;
const A_KEY: usize = 47_470_000;

fn a_process(pid: KoID, abi: Abi) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", lp).unwrap()
}

async fn mmap_at(sys: &Syscall<'_>, addr: usize, pages: usize, flags: usize) -> SysResult {
    sys.sys_mmap(addr, pages * PAGE, RW, flags, FileDesc::from(NO_FD), 0)
        .await
}

#[test]
fn the_top_is_asked_of_the_process_and_not_of_the_call_site() {
    let thirty_two = a_process(47_470, Abi::Linux32);
    let native = a_process(47_471, Abi::Linux);
    assert_eq!(
        on_a_thread(&thirty_two, |sys| Box::pin(async move { sys.task_max() })),
        TASK_SIZE_32
    );
    assert_eq!(
        on_a_thread(&native, |sys| Box::pin(async move { sys.task_max() })),
        usize::MAX,
        "a native process got a ceiling it never had"
    );
}

#[test]
fn a_32_bit_map_fixed_is_refused_at_the_top_and_granted_just_below_it() {
    // Asked as a pair: a check comparing the wrong way round, or one page
    // out, would still refuse something.
    let proc = a_process(47_472, Abi::Linux32);
    let (at_the_top, below_it, past_four_gib) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let at_the_top = mmap_at(sys, TASK_SIZE_32, 1, ANON_PRIVATE_FIXED).await;
            let below_it = mmap_at(sys, TASK_SIZE_32 - PAGE, 1, ANON_PRIVATE_FIXED).await;
            // And the errno band itself, which is what the probe was handed.
            let past = mmap_at(sys, 0xffff_f000, 1, ANON_PRIVATE_FIXED).await;
            (at_the_top, below_it, past)
        })
    });

    assert_eq!(
        at_the_top,
        Err(LxError::ENOMEM),
        "MAP_FIXED at the top of a 32-bit address space was granted"
    );
    assert_eq!(
        past_four_gib,
        Err(LxError::ENOMEM),
        "MAP_FIXED in the errno band was granted: the address it returns \
         cannot be told apart from a failure"
    );
    assert_eq!(
        below_it,
        Ok(TASK_SIZE_32 - PAGE),
        "the last page a 32-bit process may hold was refused"
    );
}

#[test]
fn a_native_map_fixed_at_the_32_bit_top_is_left_alone() {
    let proc = a_process(47_473, Abi::Linux);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move { mmap_at(sys, TASK_SIZE_32, 1, ANON_PRIVATE_FIXED).await })
    });
    assert_eq!(
        got,
        Ok(TASK_SIZE_32),
        "the 32-bit ceiling reached a native process"
    );
}

#[test]
fn a_kernel_chosen_mmap_never_leaves_a_32_bit_address_space() {
    // The placement search takes "just above the highest mapping" first, so
    // a mapping that ends AT the ceiling is what makes it want to go over.
    // That is the shape step 56 hit: enough mmaps, and the next one walks off
    // the top.
    let proc = a_process(47_474, Abi::Linux32);
    let (reserved, chosen) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let reserved = mmap_at(sys, TASK_SIZE_32 - PAGE, 1, ANON_PRIVATE_FIXED).await;
            let chosen = mmap_at(sys, 0, 2, ANON_PRIVATE).await;
            (reserved, chosen)
        })
    });
    assert_eq!(reserved, Ok(TASK_SIZE_32 - PAGE));
    let at = chosen.expect("there is room below the ceiling for two pages");
    assert!(
        at + 2 * PAGE <= TASK_SIZE_32,
        "mmap chose {:#x} for a 32-bit process, which ends above {:#x}",
        at,
        TASK_SIZE_32
    );
}

#[test]
fn an_in_place_mremap_cannot_grow_through_the_top() {
    // The one placement that chooses no address at all: the mapping just
    // stretches. Without the ceiling here it stretches past 4 GiB while the
    // program still holds the address it already had.
    let thirty_two = a_process(47_475, Abi::Linux32);
    let native = a_process(47_476, Abi::Linux);
    let at = TASK_SIZE_32 - PAGE;

    let grow = |proc: &Arc<Process>| {
        on_a_thread(proc, move |sys| {
            Box::pin(async move {
                let reserved = mmap_at(sys, at, 1, ANON_PRIVATE_FIXED).await;
                let grown = sys.sys_mremap(at, PAGE, 2 * PAGE, 0, 0);
                (reserved, grown)
            })
        })
    };

    assert_eq!(
        grow(&thirty_two),
        (Ok(at), Err(LxError::ENOMEM)),
        "an in-place mremap grew a 32-bit mapping through the top"
    );
    assert_eq!(
        grow(&native),
        (Ok(at), Ok(at)),
        "the 32-bit ceiling stopped a native mremap"
    );
}

#[test]
fn a_32_bit_brk_above_the_top_hands_back_the_old_break() {
    // `brk` says no by returning the break it still has, which is why this
    // needs a test: the refusal looks exactly like an answer.
    //
    // The break starts one page below the top, so the grow asked for here is
    // two pages and nothing else refuses it: without the ceiling it is
    // granted and the program's heap ends at `0xfffff000`, in the errno
    // band. A break further away would be refused by `brk_plan` for being
    // absurd, and this test would pass with the ceiling gone.
    let proc = a_process(47_477, Abi::Linux32);
    let heap = heap_base_for(&proc.vmar(), Abi::Linux32);
    let near_the_top = TASK_SIZE_32 - PAGE;
    assert!(near_the_top > heap);
    proc.linux().set_brk(near_the_top);
    let (above, stored, under) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let above = sys.sys_brk(TASK_SIZE_32 + PAGE);
            let stored = sys.linux_process().brk();
            // And a break that stays under it still moves, so the refusal
            // above is the ceiling's and not the whole call giving up.
            let under = sys.sys_brk(TASK_SIZE_32);
            (above, stored, under)
        })
    });
    assert_eq!(
        above,
        Ok(near_the_top),
        "a break past the top of the space was taken"
    );
    assert_eq!(stored, near_the_top, "and stored");
    assert_eq!(
        under,
        Ok(TASK_SIZE_32),
        "a break at the top of the space was refused"
    );
}

#[test]
fn a_32_bit_brk_measures_from_its_own_heap_base() {
    // The heap of a 32-bit process starts a quarter of the way up ITS address
    // space --- 1 GiB --- and `sys_brk` asked for the 64-bit answer, 32 TiB.
    // Every break an i386 program can name is below that, so `brk_target`
    // read each one as "below where the heap starts" and the break never
    // moved: a malloc that grows the heap through `brk` never grew it once.
    let proc = a_process(47_478, Abi::Linux32);
    let heap = heap_base_for(&proc.vmar(), Abi::Linux32);
    assert!(
        heap < TASK_SIZE_32,
        "a 32-bit heap base of {:#x} is not an address the process can name",
        heap
    );
    proc.linux().set_brk(heap);
    let (grown, stored) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let grown = sys.sys_brk(heap + 3 * PAGE);
            (grown, sys.linux_process().brk())
        })
    });
    assert_eq!(
        grown,
        Ok(heap + 3 * PAGE),
        "a plain three-page grow of a 32-bit heap was refused"
    );
    assert_eq!(stored, heap + 3 * PAGE);
}

#[test]
fn a_32_bit_shmat_is_refused_above_the_top() {
    // `shmat` places memory through the same VMAR call and is the fourth
    // door. Only the refusal is asked for here: the two addresses either side
    // of the ceiling would have to be really mapped, and under libos a
    // System V attach lands on a host page this binary owns.
    let proc = a_process(47_479, Abi::Linux32);
    let native = a_process(47_480, Abi::Linux);
    let answers = |proc: &Arc<Process>, key: usize| {
        on_a_thread(proc, move |sys| {
            Box::pin(async move {
                let id = sys
                    .sys_shmget(key, PAGE, IPC_CREAT | EVERYBODY)
                    .expect("a fresh one-page segment");
                sys.sys_shmat(id, TASK_SIZE_32, 0)
            })
        })
    };
    assert_eq!(
        answers(&proc, A_KEY),
        Err(LxError::ENOMEM),
        "shmat placed a 32-bit process's segment at the top of its space"
    );
    assert_ne!(
        answers(&native, A_KEY + 1),
        Err(LxError::ENOMEM),
        "the 32-bit ceiling reached shmat of a native process"
    );
}
