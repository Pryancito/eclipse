//! A thread with no slice on record lets a wake through, and the tick is what
//! starts its slice. So the trap path must ask the floor BEFORE it ticks: asked
//! after, the same wake meets a slice begun this instant and is denied
//! (`loader/src/linux.rs`, the timer-vector preempt decision).

use super::*;
use crate::task::Job;

#[test]
fn the_tick_is_what_closes_the_floor_on_a_thread_with_no_slice() {
    let proc = Process::create(&Job::root(), "proc").expect("un proceso");
    let t = Thread::create(&proc, "thread").expect("un hilo");
    t.sched.slice_end_ns.store(0, Ordering::Relaxed);
    assert!(
        t.sched_may_preempt_on_wake(),
        "sin slice registrado el despertar tiene que pasar"
    );
    assert!(!t.tick_should_preempt());
    assert!(
        !t.sched_may_preempt_on_wake(),
        "tras el tick el slice acaba de empezar: si esto pasa, el orden en \
         el trap da igual y el comentario de alli miente"
    );
}
