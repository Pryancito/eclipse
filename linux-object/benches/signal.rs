//! Reference benchmarks for the signal bookkeeping: the pending-set scan every
//! delivery check runs, the mask a handler is entered with, the `siginfo` a
//! delivery builds, and what `sigaltstack(2)` accepts.
//!
//! These are the in-kernel halves of the C suite's `sig` section. From
//! userspace, `tools/eclipse-bench` reports what a process pays for
//! `kill(self)+handler` -- about 2 us here -- with the trap, the frame, the
//! handler and the return through it all in one figure. None of that says how
//! much is this crate's own decision-making. These rows do, and the answer
//! matters: if the bookkeeping is nanoseconds, then a slow delivery is the
//! frame and the scheduler, and no amount of work here would move it.
//!
//! Nothing in this file touches a task, a scheduler or an address space, which
//! is what makes it runnable on the host at all. Delivery itself cannot be
//! benched here for that reason -- it needs a thread to deliver to -- and the
//! `psched` section of the C suite is where that is measured instead.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), for the reasons
//! given in `zircon-object/benches/vm.rs`.
//!
//! Every row passes its INPUTS through `test::black_box` as well as its
//! result. These operations are a handful of instructions over constants the
//! compiler can see, so black-boxing only the result lets it fold the work
//! away and leaves the row reporting the loop overhead -- which looks like a
//! very fast kernel and is a measurement of nothing. Sub-nanosecond figures
//! here are the signature of that mistake, not of speed.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p linux-object --bench signal --features mock-disk
//! ```

#![feature(test)]

extern crate test;

use linux_object::signal::{
    SigInfo, Signal, SignalAction, SignalActionFlags, SignalCode, SignalStack, SignalStackFlags,
    Sigset,
};
use test::Bencher;

/// A pending set with every standard signal in it, which is the worst case for
/// anything that walks the word.
fn full_standard() -> Sigset {
    let mut set = Sigset::empty();
    // Named rather than built from a numeric range: the enum is the authority
    // on which numbers exist, and a range would quietly include a gap.
    for sig in [
        Signal::SIGHUP,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGILL,
        Signal::SIGTRAP,
        Signal::SIGABRT,
        Signal::SIGBUS,
        Signal::SIGFPE,
        Signal::SIGKILL,
        Signal::SIGUSR1,
        Signal::SIGSEGV,
        Signal::SIGUSR2,
        Signal::SIGPIPE,
        Signal::SIGALRM,
        Signal::SIGTERM,
        Signal::SIGSTKFLT,
        Signal::SIGCHLD,
        Signal::SIGCONT,
        Signal::SIGSTOP,
        Signal::SIGTSTP,
        Signal::SIGTTIN,
        Signal::SIGTTOU,
        Signal::SIGURG,
        Signal::SIGXCPU,
        Signal::SIGXFSZ,
        Signal::SIGVTALRM,
        Signal::SIGPROF,
        Signal::SIGWINCH,
        Signal::SIGIO,
        Signal::SIGPWR,
        Signal::SIGSYS,
    ] {
        set.insert(sig);
    }
    set
}

#[bench]
fn sigset_insert(b: &mut Bencher) {
    b.iter(|| {
        let mut set = test::black_box(Sigset::empty());
        set.insert(test::black_box(Signal::SIGUSR1));
        test::black_box(set)
    });
}

#[bench]
fn sigset_contains(b: &mut Bencher) {
    let set = full_standard();
    b.iter(|| test::black_box(test::black_box(&set).contains(test::black_box(Signal::SIGWINCH))));
}

/// `find_first_signal` is run on every check for a deliverable signal, so it is
/// the hottest thing in this file. The pair below puts the only set bit at the
/// bottom and at the top of the word: a FLAT pair is the intrinsic
/// (`trailing_zeros`, one instruction) and anything else is a loop, which is
/// what this row exists to catch if someone ever rewrites it.
#[bench]
fn sigset_find_first_lowest_bit(b: &mut Bencher) {
    let mut set = Sigset::empty();
    set.insert(Signal::SIGHUP); // bit 0
    b.iter(|| test::black_box(test::black_box(&set).find_first_signal()));
}

#[bench]
fn sigset_find_first_highest_bit(b: &mut Bencher) {
    let mut set = Sigset::empty();
    set.insert(Signal::SIGRT63); // the top of the word
    b.iter(|| test::black_box(test::black_box(&set).find_first_signal()));
}

/// The empty set: the answer on every check that finds nothing to deliver,
/// which is almost all of them.
#[bench]
fn sigset_find_first_of_empty(b: &mut Bencher) {
    let set = Sigset::empty();
    b.iter(|| test::black_box(test::black_box(&set).find_first_signal()));
}

#[bench]
fn sigset_mask_with(b: &mut Bencher) {
    let set = full_standard();
    let mut blocked = Sigset::empty();
    blocked.insert(Signal::SIGCHLD);
    blocked.insert(Signal::SIGWINCH);
    b.iter(|| test::black_box(test::black_box(&set).mask_with(test::black_box(&blocked))));
}

/// `blockable` is the rule that SIGKILL and SIGSTOP cannot be blocked, and it
/// is applied at seven different doors, so it runs on every mask that arrives
/// from userspace.
#[bench]
fn sigset_blockable(b: &mut Bencher) {
    let set = full_standard();
    b.iter(|| test::black_box(test::black_box(&set).blockable()));
}

/// The mask a handler is entered with, computed once per delivery.
#[bench]
fn signal_action_handler_mask(b: &mut Bencher) {
    let mut action = SignalAction {
        handler: 0x4000,
        flags: SignalActionFlags::empty(),
        restorer: 0x5000,
        mask: full_standard(),
    };
    action = action.stored();
    let blocked = Sigset::empty();
    b.iter(|| {
        test::black_box(
            test::black_box(&action)
                .handler_mask(test::black_box(blocked), test::black_box(Signal::SIGUSR1)),
        )
    });
}

/// `siginfo` is built on every delivery and copied to the user stack, so its
/// construction is on the path the C suite times from outside. Three of the
/// constructors, because they fill different unions.
#[bench]
fn siginfo_from_user(b: &mut Bencher) {
    b.iter(|| {
        test::black_box(SigInfo::from_user(
            test::black_box(Signal::SIGUSR1),
            test::black_box(4242),
            test::black_box(1000),
            test::black_box(SignalCode(0)),
        ))
    });
}

#[bench]
fn siginfo_fault(b: &mut Bencher) {
    b.iter(|| {
        test::black_box(SigInfo::fault(
            test::black_box(Signal::SIGSEGV),
            test::black_box(SignalCode(1)),
            test::black_box(0xdead_0000),
        ))
    });
}

#[bench]
fn siginfo_child_state_change(b: &mut Bencher) {
    b.iter(|| {
        test::black_box(SigInfo::child_state_change(
            test::black_box(4242),
            test::black_box(1000),
            test::black_box(0),
        ))
    });
}

/// The bytes that go to the user stack: what the copy in a delivery copies.
#[bench]
fn siginfo_as_bytes(b: &mut Bencher) {
    let info = SigInfo::from_user(Signal::SIGUSR1, 4242, 1000, SignalCode(0));
    // Sum the bytes rather than take the length: the length is a compile-time
    // constant and the row would measure nothing but the loop.
    b.iter(|| {
        let bytes = test::black_box(&info).as_bytes();
        test::black_box(bytes.iter().fold(0u32, |a, b| a.wrapping_add(*b as u32)))
    });
}

/// An installed alternate stack, 64 KiB at a plausible address.
fn installed_altstack() -> SignalStack {
    SignalStack {
        sp: 0x7000_0000,
        flags: SignalStackFlags::empty(),
        size: 64 * 1024,
    }
}

/// `usable_from` decides, on every delivery with `SA_ONSTACK`, whether the
/// frame goes on the alternate stack or the current one. The two rows are the
/// two answers: a thread not on it, and a thread already on it (which
/// disqualifies the stack, and is the check that keeps a handler from
/// overwriting the frames below it).
#[bench]
fn signal_stack_usable_from_elsewhere(b: &mut Bencher) {
    let stack = installed_altstack();
    b.iter(|| test::black_box(test::black_box(&stack).usable_from(test::black_box(0x1_0000))));
}

#[bench]
fn signal_stack_usable_from_on_it(b: &mut Bencher) {
    let stack = installed_altstack();
    b.iter(|| test::black_box(test::black_box(&stack).usable_from(test::black_box(0x7000_8000))));
}

/// What `sigaltstack(2)` accepts, run once per call.
#[bench]
fn signal_stack_validate(b: &mut Bencher) {
    let stack = installed_altstack();
    b.iter(|| test::black_box(test::black_box(&stack).validate().is_ok()));
}

/// Taken on every delivery (`__save_altstack`) and put back at `sigreturn`.
#[bench]
fn signal_stack_take_for_frame(b: &mut Bencher) {
    let stack = installed_altstack();
    b.iter(|| {
        let mut live = test::black_box(stack);
        test::black_box(live.take_for_frame())
    });
}

#[bench]
fn signal_stack_restore_from_frame(b: &mut Bencher) {
    let stack = installed_altstack();
    b.iter(|| {
        let mut live = test::black_box(stack);
        live.restore_from_frame(test::black_box(stack), test::black_box(0x1_0000));
        test::black_box(live)
    });
}

/// Every syscall that takes a signal number runs this, and it is the only
/// place a bad number is refused.
#[bench]
fn signal_from_syscall_arg(b: &mut Bencher) {
    b.iter(|| test::black_box(Signal::from_syscall_arg(test::black_box(11))));
}
