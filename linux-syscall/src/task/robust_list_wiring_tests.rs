//! `set_tid_address` and the two halves of `robust_list`, asked through the
//! syscall.
//!
//! These three are how a thread tells the kernel where to look when it dies
//! holding something. `set_robust_list` registers the head of the list of
//! mutexes the thread owns, so that `futex_cleanup` can mark them
//! `FUTEX_OWNER_DIED` and wake a waiter instead of leaving it blocked for
//! ever; `set_tid_address` registers the word that thread exit zeroes and
//! wakes, which is what `pthread_join` is waiting on.
//!
//! The walk at exit is covered where it lives, in `linux-object`. What
//! nothing covered is the three syscalls that feed it: whether the length
//! gate is there at all, whether a refused registration leaves the old one
//! alone, whether `get_robust_list` reads the thread it was asked about,
//! and whether `set_tid_address` answers the thread rather than the process.
//!
//! Fixed pids live in 47_13x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// A tid nothing in this binary has.
const A_MISSING_TID: i32 = 999_999;

/// What the out words are seeded with, so that "wrote nothing" cannot pass
/// for "wrote zero".
const A_SEED: usize = 0xdead_beef;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// The pair of out pointers `get_robust_list` writes through, and what the
/// two words read afterwards.
struct Answer {
    head: UserOutPtr<RobustList>,
    len: usize,
}

impl Answer {
    fn seeded() -> Self {
        Answer {
            head: A_SEED.into(),
            len: A_SEED,
        }
    }

    fn head_ptr(&mut self) -> UserOutPtr<UserOutPtr<RobustList>> {
        UserOutPtr::from(&mut self.head as *mut UserOutPtr<RobustList> as usize)
    }

    fn len_ptr(&mut self) -> UserOutPtr<usize> {
        UserOutPtr::from(&mut self.len as *mut usize as usize)
    }

    fn read(&self) -> (usize, usize) {
        (self.head.as_addr(), self.len)
    }
}

#[test]
fn set_robust_list_takes_the_length_of_the_struct_and_nothing_else() {
    // `if (len != sizeof(*head)) return -EINVAL`, and it is the whole of
    // the call's validation: the head itself is a user address this kernel
    // never dereferences until the thread dies. A length gate that let a
    // short one through would have `futex_cleanup` read three words out of
    // a one-word object at exit, in the process's own memory.
    let proc = a_process(47_130, "robust-length");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut a_list = RobustList::default();
            let head = &mut a_list as *mut RobustList as usize;
            let right = size_of::<RobustList>();

            let mut nothing_yet = Answer::seeded();
            assert_eq!(
                sys.sys_get_robust_list(0, nothing_yet.head_ptr(), nothing_yet.len_ptr()),
                Ok(0)
            );
            // The length is the struct's whatever was registered, as on
            // Linux: it is the head that says there is none.
            assert_eq!(
                nothing_yet.read(),
                (0, right),
                "a thread that never registered a list has one"
            );

            for wrong in [0, 8, right - 1, right + 1, right * 2] {
                assert_eq!(
                    sys.sys_set_robust_list(UserInPtr::from(head), wrong),
                    Err(LxError::EINVAL),
                    "a robust list of the wrong length was accepted"
                );
            }

            // And none of the five stored anything: the gate is before the
            // store, so a refusal cannot leave the kernel walking a list
            // whose length it rejected.
            let mut after = Answer::seeded();
            assert_eq!(
                sys.sys_get_robust_list(0, after.head_ptr(), after.len_ptr()),
                Ok(0)
            );
            assert_eq!(
                after.read(),
                (0, right),
                "a refused set_robust_list registered the list anyway"
            );

            assert_eq!(sys.sys_set_robust_list(UserInPtr::from(head), right), Ok(0));
            let mut back = Answer::seeded();
            assert_eq!(
                sys.sys_get_robust_list(0, back.head_ptr(), back.len_ptr()),
                Ok(0)
            );
            assert_eq!(
                back.read(),
                (head, right),
                "get_robust_list did not answer what set_robust_list stored"
            );
        })
    });
}

#[test]
fn get_robust_list_reads_the_thread_it_was_asked_about_and_not_the_caller() {
    // The pid argument exists so that a debugger can read another thread's
    // list, and a call that ignored it would answer the caller's -- right
    // every time the caller asks about itself, which is every ordinary
    // call. So the two threads are given different lists and the answer
    // has to tell them apart.
    let proc = a_process(47_131, "robust-another");
    let sibling = Thread::create_linux(&proc).unwrap();
    let mut their_list = RobustList::default();
    let theirs = &mut their_list as *mut RobustList as usize;
    sibling.set_robust_list(UserInPtr::from(theirs), size_of::<RobustList>());
    let sibling_tid = sibling.id() as i32;
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut my_list = RobustList::default();
            let mine = &mut my_list as *mut RobustList as usize;
            assert_ne!(mine, theirs, "both threads were given the same list");
            assert_eq!(
                sys.sys_set_robust_list(UserInPtr::from(mine), size_of::<RobustList>()),
                Ok(0)
            );

            let mut theirs_back = Answer::seeded();
            assert_eq!(
                sys.sys_get_robust_list(sibling_tid, theirs_back.head_ptr(), theirs_back.len_ptr()),
                Ok(0)
            );
            assert_eq!(
                theirs_back.read(),
                (theirs, size_of::<RobustList>()),
                "get_robust_list of another thread answered the caller's list"
            );

            let mut mine_back = Answer::seeded();
            assert_eq!(
                sys.sys_get_robust_list(0, mine_back.head_ptr(), mine_back.len_ptr()),
                Ok(0)
            );
            assert_eq!(
                mine_back.read(),
                (mine, size_of::<RobustList>()),
                "a zero pid did not mean the calling thread"
            );
        })
    });
}

#[test]
fn a_tid_that_names_no_thread_is_esrch_and_the_two_words_stay_untouched() {
    // A negative tid and a missing one come back by different roads -- one
    // is refused on the argument, the other is a lookup that finds nobody
    // -- and both have to leave the caller's buffer alone, because a
    // caller that got ESRCH will not read it and the words it would have
    // found there are another thread's.
    let proc = a_process(47_132, "robust-esrch");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            for tid in [-1, A_MISSING_TID, i32::MIN] {
                let mut answer = Answer::seeded();
                assert_eq!(
                    sys.sys_get_robust_list(tid, answer.head_ptr(), answer.len_ptr()),
                    Err(LxError::ESRCH)
                );
                assert_eq!(
                    answer.read(),
                    (A_SEED, A_SEED),
                    "a refused get_robust_list wrote into the caller's buffer"
                );
            }
        })
    });
}

#[test]
fn set_tid_address_answers_the_calling_thread_and_keeps_the_word_it_was_given() {
    // It returns the TID -- `return task_pid_vnr(current)` -- not the pid,
    // and the difference only shows on a thread that is not the leader.
    // The word it stores is what thread exit zeroes and wakes, which is
    // how `pthread_join` learns the thread is gone, so storing it is the
    // entire point of the call.
    let proc = a_process(47_133, "tid-address");
    let leader = Thread::create_linux(&proc).unwrap();
    assert_eq!(
        leader.id(),
        47_133,
        "the first thread wears the process pid"
    );
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut word: i32 = 0;
            let at = &mut word as *mut i32 as usize;

            let tid = sys.sys_set_tid_address(UserOutPtr::from(at)).unwrap();
            assert_ne!(tid, 47_133, "set_tid_address answered the process id");
            assert_eq!(
                tid,
                sys.sys_gettid().unwrap(),
                "set_tid_address answered an id that is not the calling thread's"
            );
            assert_eq!(
                sys.thread.tid_address().as_addr(),
                at,
                "set_tid_address did not keep the word it was given"
            );

            // A null one is how a thread says "nobody is joining me", and
            // it is stored like any other: never an error.
            assert_eq!(sys.sys_set_tid_address(UserOutPtr::from(0usize)), Ok(tid));
            assert_eq!(
                sys.thread.tid_address().as_addr(),
                0,
                "a null tidptr did not clear the word"
            );
        })
    });
}
