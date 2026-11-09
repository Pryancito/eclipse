//! The loader is the code around user mode: it builds the signal frame a
//! handler returns from, turns a CPU exception into a signal, and names
//! the process in `ps`. None of it needs a process or a scheduler once the
//! decisions are lifted out, and none of it had a test before.

use super::*;
use linux_object::signal::SignalStackFlags;

const STACK_BASE: usize = 0x7fff_0000_0000;
const STACK_TOP: usize = 0x7fff_0001_0000;
const ALT_BASE: usize = 0x7000_0000;
const ALT_SIZE: usize = 0x4000;

fn alt_stack() -> SignalStack {
    SignalStack {
        sp: ALT_BASE,
        flags: SignalStackFlags::empty(),
        size: ALT_SIZE,
    }
}

// ---- where the signal frame goes -------------------------------------

#[test]
fn without_sa_onstack_the_frame_stays_below_the_red_zone() {
    // The x86-64 SysV red zone is memory a leaf function may be using
    // right now without having moved `rsp`. Building the frame at `sp`
    // itself would write over the interrupted function's live locals,
    // and the damage only shows after the handler returns.
    let sp = STACK_TOP - 0x1000;
    let top = sigframe_top(sp, &alt_stack(), false).unwrap();
    assert!(top < sp, "the frame must start below the stack pointer");
    assert_eq!(sp - top, RED_ZONE_MAX_SIZE);
}

#[test]
fn sa_onstack_moves_the_frame_to_the_top_of_the_alternate_stack() {
    // This is the whole of `sigaltstack(2)`. The alternate stack was
    // stored by the syscall and read by nobody, so every handler ran on
    // the interrupted stack -- including the SIGSEGV handler that Rust's
    // runtime and glibc install precisely to survive a blown one.
    let sp = STACK_TOP - 0x1000;
    let top = sigframe_top(sp, &alt_stack(), true).unwrap();
    assert_eq!(top, ALT_BASE + ALT_SIZE);
    assert!(
        !(STACK_BASE..=STACK_TOP).contains(&top),
        "the frame must leave the stack that raised the signal"
    );
}

#[test]
fn the_alternate_stack_keeps_its_whole_size_because_it_has_no_red_zone() {
    // The red zone protects frames that are already there. The alternate
    // stack has none -- nothing is running on it yet -- so subtracting it
    // would just waste the top 256 bytes of a stack a program may have
    // sized at exactly MINSIGSTKSZ.
    let top = sigframe_top(STACK_TOP, &alt_stack(), true).unwrap();
    assert_eq!(top, ALT_BASE + ALT_SIZE, "no red zone on the alt stack");
}

#[test]
fn sa_onstack_without_an_installed_stack_falls_back_to_the_current_one() {
    // A handler may carry SA_ONSTACK while the thread never called
    // `sigaltstack`. Linux runs it on the ordinary stack rather than
    // refusing, so a library that sets the flag unconditionally works.
    let sp = STACK_TOP - 0x1000;
    let top = sigframe_top(sp, &SignalStack::default(), true).unwrap();
    assert_eq!(top, sp - RED_ZONE_MAX_SIZE);
}

#[test]
fn a_handler_already_on_the_alternate_stack_is_not_moved_to_its_top() {
    // The nested case: a signal arrives while a handler is running on the
    // alternate stack. Restarting at the top would drop the new frame on
    // top of the live one, so the second handler would return into
    // rubble. Linux keeps the frame where the running handler left `sp`.
    let inside = ALT_BASE + ALT_SIZE / 2;
    let top = sigframe_top(inside, &alt_stack(), true).unwrap();
    assert_eq!(top, inside - RED_ZONE_MAX_SIZE);
}

#[test]
fn a_stack_pointer_too_low_to_step_past_the_red_zone_has_nowhere_to_go() {
    // `sp` is whatever the interrupted program left in the stack
    // register, and a program may leave zero there. The subtraction used
    // to be unchecked: in a debug kernel that is a panic reachable from
    // any process that installs a handler and wrecks its own `sp`, and in
    // a release kernel it wraps to the top of the address space and the
    // frame is written there.
    assert_eq!(sigframe_top(0, &alt_stack(), false), None);
    assert_eq!(
        sigframe_top(RED_ZONE_MAX_SIZE - 1, &alt_stack(), false),
        None
    );
    assert_eq!(
        sigframe_top(RED_ZONE_MAX_SIZE, &alt_stack(), false),
        Some(0)
    );
}

#[test]
fn an_alternate_stack_that_ends_past_the_address_space_has_nowhere_to_go() {
    // `sigaltstack` takes the base and the size separately, so a program
    // can name a stack whose top does not exist.
    let overflowing = SignalStack {
        sp: usize::MAX - 0x10,
        flags: SignalStackFlags::empty(),
        size: 0x1000,
    };
    assert_eq!(sigframe_top(STACK_TOP, &overflowing, true), None);
}

// ---- and whether it can go there at all ------------------------------

#[test]
fn a_frame_inside_one_writable_mapping_is_accepted() {
    let maps = [(STACK_BASE, STACK_TOP, true)];
    assert!(frame_fits_writable(&maps, STACK_TOP - 0x1000, 0x400));
    // Flush against both ends of the mapping is still inside it.
    assert!(frame_fits_writable(&maps, STACK_BASE, 0x400));
    assert!(frame_fits_writable(&maps, STACK_TOP - 0x400, 0x400));
}

#[test]
fn a_frame_that_runs_off_the_end_of_its_mapping_is_refused() {
    // The guard page at the bottom of a thread stack is exactly this: the
    // frame starts on a real page and ends on one that is not there.
    let maps = [(STACK_BASE, STACK_TOP, true)];
    assert!(!frame_fits_writable(&maps, STACK_TOP - 0x10, 0x400));
    assert!(!frame_fits_writable(&maps, STACK_BASE - 0x10, 0x400));
}

#[test]
fn a_frame_one_byte_past_its_mapping_is_refused() {
    // Both tests above overshoot by 0x3f0 bytes, so an off-by-one in
    // either bound passes them: the frame is still a long way outside.
    // One byte is where `access_ok` is actually decided, and one byte
    // past the top of the stack is one byte into the next mapping or
    // into nothing at all.
    let maps = [(STACK_BASE, STACK_TOP, true)];
    assert!(!frame_fits_writable(&maps, STACK_TOP - 0x400 + 1, 0x400));
    assert!(!frame_fits_writable(&maps, STACK_BASE - 1, 0x400));
    // And one byte inside each bound still fits, so the refusals above
    // are the bound and not a blanket no.
    assert!(frame_fits_writable(&maps, STACK_TOP - 0x400, 0x400));
    assert!(frame_fits_writable(&maps, STACK_BASE + 1, 0x400));
}

#[test]
fn a_frame_in_a_read_only_mapping_is_refused() {
    // A program that points `sp` at its own text segment. The kernel
    // writes the frame through a raw pointer with the user page table
    // live, so without this the write either faults in kernel mode or,
    // where the mapping is writable to the kernel, succeeds.
    let maps = [(0x40_0000, 0x41_0000, false), (STACK_BASE, STACK_TOP, true)];
    assert!(!frame_fits_writable(&maps, 0x40_1000, 0x400));
    assert!(frame_fits_writable(&maps, STACK_BASE + 0x1000, 0x400));
}

#[test]
fn a_frame_in_no_mapping_at_all_is_refused() {
    let maps = [(STACK_BASE, STACK_TOP, true)];
    // An address the process never mapped, and the kernel half of the
    // address space, which is what a hostile `sp` aims at.
    assert!(!frame_fits_writable(&maps, 0x1_0000, 0x400));
    assert!(!frame_fits_writable(&maps, 0xffff_8000_0000_0000, 0x400));
    assert!(!frame_fits_writable(&[], STACK_BASE, 0x400));
}

#[test]
fn a_frame_spanning_two_adjacent_mappings_is_refused() {
    // Treating touching mappings as one would mean trusting that they
    // really do touch, and a real stack never straddles the boundary
    // anyway: the conservative answer costs a process nothing.
    let maps = [
        (STACK_BASE, STACK_BASE + 0x1000, true),
        (STACK_BASE + 0x1000, STACK_TOP, true),
    ];
    assert!(!frame_fits_writable(&maps, STACK_BASE + 0xf00, 0x400));
}

#[test]
fn a_frame_whose_length_wraps_the_address_space_is_refused() {
    let maps = [(0, usize::MAX, true)];
    assert!(!frame_fits_writable(&maps, usize::MAX - 0x10, 0x400));
}

// ---- the pushes themselves -------------------------------------------

#[test]
fn push_stack_writes_below_the_pointer_it_is_given() {
    // A stack grows down, so the value must land *under* `stack_top` and
    // the returned address must be where it landed -- that address is
    // what the handler is handed as its `siginfo`/`ucontext` argument.
    let mut buf = [0u64; 8];
    let top = buf.as_mut_ptr() as usize + core::mem::size_of_val(&buf);
    let at = push_stack(top, 0xdead_beef_u64);
    assert_eq!(at, top - 8, "one object below the top");
    assert_eq!(buf[7], 0xdead_beef);
    assert_eq!(buf[6], 0, "nothing else was touched");

    let again = push_stack(at, 0x1234_u64);
    assert_eq!(again, at - 8);
    assert_eq!(buf[6], 0x1234);
    assert_eq!(buf[7], 0xdead_beef, "the first push survived the second");
}

#[test]
fn the_frame_budget_covers_everything_the_handler_path_pushes() {
    // `frame_fits_writable` is asked about a fixed budget, so the budget
    // has to be at least what the pushes below it actually use. Each push
    // first rounds the pointer down to 16, which can cost 15 bytes, and
    // on x86-64 a return address goes on top of the two structs.
    let actual = core::mem::size_of::<SigInfo>()
        + core::mem::size_of::<SignalUserContext>()
        + core::mem::size_of::<usize>()
        + 3 * 15;
    let budget = RED_ZONE_MAX_SIZE
        + core::mem::size_of::<SigInfo>()
        + core::mem::size_of::<SignalUserContext>()
        + core::mem::size_of::<usize>();
    assert!(
        budget >= actual,
        "budget {} does not cover the {} bytes pushed",
        budget,
        actual
    );
}

// ---- CPU exceptions --------------------------------------------------

#[test]
#[cfg(target_arch = "x86_64")]
fn cpu_exceptions_map_to_the_signals_linux_raises() {
    // `arch/x86/kernel/traps.c`. Three of these are SIGBUS or SIGFPE
    // where the neighbours are SIGSEGV, and a program that installs a
    // per-signal handler -- a JIT catching #DE, a runtime catching
    // #GP -- sees the difference directly.
    assert_eq!(cpu_fault_signal(0x00), Signal::SIGFPE, "#DE divide error");
    assert_eq!(cpu_fault_signal(0x04), Signal::SIGSEGV, "#OF overflow");
    assert_eq!(cpu_fault_signal(0x05), Signal::SIGSEGV, "#BR bound range");
    assert_eq!(cpu_fault_signal(0x07), Signal::SIGFPE, "#NM no FPU");
    assert_eq!(cpu_fault_signal(0x08), Signal::SIGKILL, "#DF double fault");
    assert_eq!(cpu_fault_signal(0x09), Signal::SIGFPE, "coproc overrun");
    assert_eq!(cpu_fault_signal(0x0a), Signal::SIGSEGV, "#TS invalid TSS");
    assert_eq!(cpu_fault_signal(0x0b), Signal::SIGBUS, "#NP not present");
    assert_eq!(cpu_fault_signal(0x0c), Signal::SIGBUS, "#SS stack segment");
    assert_eq!(cpu_fault_signal(0x0d), Signal::SIGSEGV, "#GP protection");
    assert_eq!(cpu_fault_signal(0x10), Signal::SIGFPE, "#MF x87");
    assert_eq!(cpu_fault_signal(0x13), Signal::SIGFPE, "#XF SIMD");
}

#[test]
#[cfg(target_arch = "x86_64")]
fn an_unknown_exception_number_is_a_segmentation_fault() {
    // The table is indexed by `trap_num as u8`, so a number above 255
    // aliases onto a real vector. Anything unrecognised has to end at a
    // signal rather than at a `match` that cannot answer.
    assert_eq!(cpu_fault_signal(0xff), Signal::SIGSEGV);
    assert_eq!(cpu_fault_signal(usize::MAX), Signal::SIGSEGV);
    // 0x100 truncates to 0x00, the divide error: the truncation is
    // deliberate (a vector is one byte) but worth pinning, because a
    // reader who expects the default arm here would be wrong.
    assert_eq!(cpu_fault_signal(0x100), Signal::SIGFPE);
}

// ---- the process name ------------------------------------------------

#[test]
fn the_process_name_is_the_last_path_component() {
    // This is what `ps` and `/proc/<pid>/comm` show, and what a user
    // greps for to kill something.
    assert_eq!(comm_from_path("/usr/bin/firefox"), "firefox");
    assert_eq!(comm_from_path("firefox"), "firefox");
    assert_eq!(comm_from_path("./a.out"), "a.out");
    assert_eq!(comm_from_path("/usr/lib/x86_64/ld.so"), "ld.so");
}

#[test]
fn a_path_with_no_last_component_does_not_lose_its_name() {
    // A trailing slash and the root itself both leave an empty component.
    // Empty is what the caller gets; it must not panic, because the path
    // comes from `execve` and a program may pass anything.
    assert_eq!(comm_from_path("/usr/bin/"), "");
    assert_eq!(comm_from_path("/"), "");
    assert_eq!(comm_from_path(""), "");
}
