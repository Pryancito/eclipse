//! The contexts a thread saved before running its signal handlers, innermost
//! last, and how `sigreturn` finds its own among them.
//!
//! Linux keeps none of this in the kernel: the interrupted registers live in
//! the signal frame on the user stack, and `rt_sigreturn` reads them back from
//! the frame its stack pointer is at. Here the frame on the user stack carries
//! the registers a handler may edit, and the rest -- the FPU state above all,
//! and how the thread entered the kernel -- stays in the kernel, saved when the
//! frame is built.
//!
//! It was ONE slot. That was safe only while nothing could be delivered to a
//! thread already inside a handler, and that gate is gone (it livelocked any
//! thread that got a second signal mid-handler; see
//! `LinuxThread::handling_signal`). Since then a second signal nests, as in
//! Linux, and its frame overwrote the outer one's: the inner handler returned
//! fine, and the OUTER handler's `sigreturn` found the slot empty and answered
//! `EINVAL`. A `sigreturn` that fails returns: the thread ran on past the
//! `syscall` instruction of libc's restorer into whatever code follows it.
//! A SIGCHLD handler interrupted by a SIGWINCH, a SIGALRM handler interrupted
//! by a SIGTERM -- any two handlers that do not mask each other.
//!
//! So it is a stack, and a frame is matched by the stack pointer `sigreturn`
//! is made with, which is where Linux looks too. That is what keeps a handler
//! left with `siglongjmp` from leaving its frame in the way of everyone after
//! it: an inner handler abandoned that way is dropped when the outer one
//! returns past it, and one abandoned for good stays at the bottom until the
//! bound pushes it out.

use alloc::collections::VecDeque;
use kernel_hal::context::UserContext;

/// What one signal frame saved: the context to go back to, the user addresses
/// of the `siginfo_t` and `ucontext_t` the handler was handed, and the stack
/// pointer its `sigreturn` will be made with.
#[derive(Clone, Copy)]
struct SavedFrame {
    context: UserContext,
    siginfo: usize,
    uctx: usize,
    return_sp: usize,
}

/// The frames of the handlers a thread is inside, outermost first.
#[derive(Clone, Default)]
pub(super) struct SignalFrames(VecDeque<SavedFrame>);

impl SignalFrames {
    /// How many frames are kept. Nesting needs one distinct unblocked signal
    /// per level (a handler blocks its own signal unless `SA_NODEFER`), so a
    /// real program is a handful deep; what the bound is for is the frames of
    /// handlers that never return, which a `siglongjmp` out of one leaves
    /// behind for good. The oldest goes first.
    pub(super) const LIMIT: usize = 32;

    /// Save the frame of a handler about to be entered.
    pub(super) fn push(
        &mut self,
        context: UserContext,
        siginfo: usize,
        uctx: usize,
        return_sp: usize,
    ) {
        if self.0.len() == Self::LIMIT {
            self.0.pop_front();
        }
        self.0.push_back(SavedFrame {
            context,
            siginfo,
            uctx,
            return_sp,
        });
    }

    /// The frame a `sigreturn` made with stack pointer `sp` returns from.
    ///
    /// The innermost frame saved for that stack pointer, and every frame
    /// saved after it goes too: those are handlers that were left without a
    /// `sigreturn`, and the one returning now is outside them. A stack
    /// pointer that matches nothing takes the innermost frame, which is what
    /// the single slot did for every call.
    pub(super) fn take(&mut self, sp: usize) -> Option<(UserContext, usize, usize)> {
        let at = self
            .0
            .iter()
            .rposition(|frame| frame.return_sp == sp)
            .or_else(|| self.0.len().checked_sub(1))?;
        self.0.truncate(at + 1);
        let frame = self.0.pop_back()?;
        Some((frame.context, frame.siginfo, frame.uctx))
    }

    /// Forget every frame: they name an image `execve` has replaced.
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel_hal::context::UserContextField;

    /// A context told apart from the others by its program counter.
    fn at(pc: usize) -> UserContext {
        let mut ctx = UserContext::default();
        ctx.set_field(UserContextField::InstrPointer, pc);
        ctx
    }

    fn pc_of(taken: Option<(UserContext, usize, usize)>) -> Option<usize> {
        taken.map(|(mut ctx, _, _)| ctx.get_field(UserContextField::InstrPointer))
    }

    #[test]
    fn a_handler_inside_another_does_not_take_the_outer_frame_with_it() {
        // The main program is interrupted by SIGCHLD; its handler, by SIGWINCH.
        let mut frames = SignalFrames::default();
        frames.push(at(0x40_1000), 0x7ff0_0f00, 0x7ff0_0e00, 0x7ff0_0d00);
        frames.push(at(0x40_2000), 0x7ff0_0b00, 0x7ff0_0a00, 0x7ff0_0900);

        assert_eq!(
            pc_of(frames.take(0x7ff0_0900)),
            Some(0x40_2000),
            "the inner sigreturn goes back into the SIGCHLD handler"
        );
        assert_eq!(
            pc_of(frames.take(0x7ff0_0d00)),
            Some(0x40_1000),
            "the outer sigreturn found nothing: one slot, overwritten by the inner frame"
        );
        assert!(
            frames.take(0x7ff0_0d00).is_none(),
            "nothing left to return to"
        );
    }

    #[test]
    fn the_frame_comes_back_with_the_addresses_it_was_saved_with() {
        let mut frames = SignalFrames::default();
        frames.push(at(0x40_1000), 0x7ff0_0f00, 0x7ff0_0e00, 0x7ff0_0d00);
        let (_, siginfo, uctx) = frames.take(0x7ff0_0d00).unwrap();
        assert_eq!((siginfo, uctx), (0x7ff0_0f00, 0x7ff0_0e00));
    }

    #[test]
    fn an_inner_handler_left_with_siglongjmp_is_dropped_when_the_outer_one_returns() {
        // The inner handler jumps back into the outer one, which then returns
        // normally. Taking the innermost frame would put the thread back where
        // the inner signal interrupted the outer handler, for the second time.
        let mut frames = SignalFrames::default();
        frames.push(at(0x40_1000), 0, 0, 0x7ff0_0d00);
        frames.push(at(0x40_2000), 0, 0, 0x7ff0_0900);

        assert_eq!(pc_of(frames.take(0x7ff0_0d00)), Some(0x40_1000));
        assert_eq!(frames.len(), 0, "the abandoned inner frame is still there");
    }

    #[test]
    fn a_handler_left_for_good_does_not_get_in_the_way_of_later_ones() {
        // `siglongjmp` from a handler back to the main program: that frame is
        // never returned from. The next handler's frame lands at the same
        // place on the stack, and must be the one its `sigreturn` finds.
        let mut frames = SignalFrames::default();
        frames.push(at(0x40_1000), 0, 0, 0x7ff0_0d00);
        frames.push(at(0x40_3000), 0, 0, 0x7ff0_0d00);

        assert_eq!(pc_of(frames.take(0x7ff0_0d00)), Some(0x40_3000));
    }

    #[test]
    fn a_stack_pointer_that_matches_nothing_takes_the_innermost_frame() {
        let mut frames = SignalFrames::default();
        frames.push(at(0x40_1000), 0, 0, 0x7ff0_0d00);
        frames.push(at(0x40_2000), 0, 0, 0x7ff0_0900);

        assert_eq!(pc_of(frames.take(0x1234)), Some(0x40_2000));
        assert_eq!(frames.len(), 1, "only the innermost frame went");
        assert_eq!(pc_of(frames.take(0x1234)), Some(0x40_1000));
    }

    #[test]
    fn past_the_bound_the_oldest_frame_goes() {
        let sp = |i: usize| 0x7ff0_0000 - i * 0x100;
        let mut frames = SignalFrames::default();
        for i in 0..=SignalFrames::LIMIT {
            frames.push(at(0x40_0000 + i), 0, 0, sp(i));
        }
        assert_eq!(frames.len(), SignalFrames::LIMIT);
        // The second one pushed is the outermost now: returning from it
        // leaves nothing behind.
        assert_eq!(pc_of(frames.take(sp(1))), Some(0x40_0001));
        assert_eq!(frames.len(), 0, "the oldest frame was kept past the bound");
    }

    /// The same, through the `Thread` methods `fork`, `sigreturn` and
    /// `execve` call.
    fn a_thread() -> alloc::sync::Arc<crate::task::Thread> {
        let root = crate::task::Job::root();
        let proc = crate::task::Process::create(&root, "proc").expect("a process");
        crate::task::Thread::create(&proc, "thread").expect("a thread")
    }

    #[test]
    fn sigreturn_finds_its_frame_by_the_stack_pointer_it_is_made_with() {
        let t = a_thread();
        t.backup_context(at(0x40_1000), 0, 0, 0x7ff0_0d00);
        t.backup_context(at(0x40_2000), 0, 0, 0x7ff0_0900);
        assert_eq!(
            pc_of(t.fetch_backup_context(0x7ff0_0d00)),
            Some(0x40_1000),
            "the outer handler, returning past an inner one it jumped out of"
        );
    }

    #[test]
    fn a_child_forked_inside_a_handler_returns_from_it() {
        let parent = a_thread();
        parent.backup_context(at(0x40_1000), 0x7ff0_0f00, 0x7ff0_0e00, 0x7ff0_0d00);
        let child = a_thread();
        child.inherit_backup_contexts(&parent);

        assert_eq!(
            pc_of(child.fetch_backup_context(0x7ff0_0d00)),
            Some(0x40_1000)
        );
        assert_eq!(
            pc_of(parent.fetch_backup_context(0x7ff0_0d00)),
            Some(0x40_1000),
            "the child took the parent's frame instead of copying it"
        );
    }

    #[test]
    fn a_thread_that_execs_has_no_handler_to_return_from() {
        let t = a_thread();
        t.backup_context(at(0x40_1000), 0, 0, 0x7ff0_0d00);
        t.forget_backup_contexts();
        assert!(t.fetch_backup_context(0x7ff0_0d00).is_none());
    }

    #[test]
    fn exec_forgets_every_frame() {
        let mut frames = SignalFrames::default();
        frames.push(at(0x40_1000), 0, 0, 0x7ff0_0d00);
        frames.push(at(0x40_2000), 0, 0, 0x7ff0_0900);
        frames.clear();
        assert!(frames.take(0x7ff0_0d00).is_none());
    }

    /// `execve` replaces the image every frame points into, so they all go,
    /// not only the one a `sigreturn` would take next. Forgetting just the
    /// innermost left the outer handler's frame behind, and the frame a
    /// `sigreturn` of the NEW image takes when its stack pointer matches
    /// nothing is the innermost one left: the thread would have gone back to
    /// an address of the replaced image, with its registers. The test above
    /// says `clear` empties the queue; this one says the thread's own way in
    /// empties it too, which is the call `execve` makes.
    #[test]
    fn exec_forgets_the_outer_frames_too() {
        let t = a_thread();
        t.backup_context(at(0x40_1000), 0, 0, 0x7ff0_0d00);
        t.backup_context(at(0x40_2000), 0, 0, 0x7ff0_0900);

        t.forget_backup_contexts();

        assert!(
            t.fetch_backup_context(0x7ff0_0d00).is_none(),
            "the outer handler's frame survived the exec"
        );
        assert!(t.fetch_backup_context(0x7ff0_0900).is_none());
    }

    /// The two user addresses a frame carries come back the way they went in.
    /// They are not interchangeable: the `ucontext_t` is where `sigreturn`
    /// reads the mask to restore, and the `siginfo_t` is what a handler with
    /// `SA_SIGINFO` was handed, so swapping them restores a mask read out of
    /// the wrong structure. [`SignalFrames`] keeps them apart; this says the
    /// thread's own way in and out does too, and the tests that go through the
    /// thread all passed zeros for both.
    #[test]
    fn a_threads_frame_keeps_the_two_addresses_apart() {
        let t = a_thread();
        t.backup_context(at(0x40_1000), 0x7ff0_0f00, 0x7ff0_0e00, 0x7ff0_0d00);

        let (_, siginfo, uctx) = t.fetch_backup_context(0x7ff0_0d00).unwrap();

        assert_eq!((siginfo, uctx), (0x7ff0_0f00, 0x7ff0_0e00));
    }
}
