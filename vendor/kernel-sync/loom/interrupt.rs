//! The interrupt layer the locks sit on, for the loom build only.
//!
//! Loom runs every model thread as a coroutine on one OS thread, so the
//! host backend's `std::thread_local!` CPU slots would be one slot shared by
//! all of them, and their `push_off`/`pop_off` stacks would interleave into
//! nonsense. Interrupt masking is a per-CPU affair anyway and has nothing to
//! say about what one CPU sees of another's atomics, which is all loom
//! checks; so here it is only a per-thread nesting count, kept honest by an
//! assertion, so the models still catch a guard that pops what it never
//! pushed.

use core::cell::Cell;

loom::thread_local! {
    static DEPTH: Cell<i32> = Cell::new(0);
}

pub fn current_cpu_id() -> u8 {
    0
}

pub fn current_cpu_id_via_apic() -> u8 {
    0
}

pub fn lock_depth() -> i32 {
    DEPTH.with(|d| d.get())
}

pub(crate) fn push_off() -> u8 {
    DEPTH.with(|d| d.set(d.get() + 1));
    0
}

pub(crate) fn push_off_short() -> Result<u8, bool> {
    Ok(push_off())
}

pub(crate) fn push_off_long(_old: bool) -> u8 {
    push_off()
}

pub(crate) fn pop_off() {
    DEPTH.with(|d| {
        assert!(d.get() > 0, "pop_off without a push_off on this thread");
        d.set(d.get() - 1);
    });
}
