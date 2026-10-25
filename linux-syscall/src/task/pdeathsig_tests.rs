use super::*;
use linux_object::signal::Signal as LinuxSignal;

#[test]
fn zero_disarms() {
    assert_eq!(pdeathsig_from_arg(0), Ok(0));
}

#[test]
fn every_signal_is_latched_as_itself() {
    for n in 1..=LinuxSignal::RTMAX {
        assert_eq!(pdeathsig_from_arg(n), Ok(n as u8), "PR_SET_PDEATHSIG {}", n);
    }
}

#[test]
fn a_number_above_nsig_is_rejected_not_truncated() {
    // 265 & 0xff == 9: the child asked for a signal that does not exist and
    // got SIGKILL on its parent's death.
    assert_eq!(pdeathsig_from_arg(265), Err(LxError::EINVAL));
    assert_eq!(pdeathsig_from_arg(65), Err(LxError::EINVAL));
    assert_eq!(pdeathsig_from_arg(256), Err(LxError::EINVAL));
}

#[test]
fn prctl_does_not_truncate_to_int_the_way_kill_does() {
    // prctl(2) declares arg2 `unsigned long`, so unlike kill(2) there is no
    // 32-bit narrowing before `valid_signal()`: the high half makes the
    // number out of range instead of disappearing.
    assert_eq!(
        pdeathsig_from_arg(0xdead_beef_0000_0009),
        Err(LxError::EINVAL)
    );
    assert_eq!(pdeathsig_from_arg(0x1_0000_0000), Err(LxError::EINVAL));
    // The same two words through kill(2)'s rule, for contrast.
    assert_eq!(
        LinuxSignal::from_syscall_arg(0xdead_beef_0000_0009),
        Ok(Some(LinuxSignal::SIGKILL))
    );
    assert_eq!(LinuxSignal::from_syscall_arg(0x1_0000_0000), Ok(None));
}

#[test]
fn a_negative_argument_is_out_of_range() {
    // prctl never sees it as negative; it sees a very large unsigned.
    assert_eq!(pdeathsig_from_arg(usize::MAX), Err(LxError::EINVAL));
    assert_eq!(pdeathsig_from_arg(-9i64 as usize), Err(LxError::EINVAL));
}

/// A fork records the thread it was made from: that thread's exit, and not
/// only its whole process's, is the parent death the child asked to hear of.
#[test]
fn a_fork_remembers_the_thread_it_came_from() {
    use crate::on_a_thread::on_a_thread;
    use alloc::boxed::Box;
    use linux_object::process::{find_task, LinuxProcess};
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::ROOT_JOB;

    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        45_465,
        "padre",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let (recorded, forker) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let child = sys.sys_fork(0, 0).unwrap();
            // The child's thread has nothing to run here and may have
            // exited already, so it can be a zombie its parent keeps.
            let child = find_task(child as KoID).unwrap();
            (child.linux().forking_thread(), sys.thread.id())
        })
    });
    assert_eq!(recorded, forker);
}

/// That thread leaving while another goes on is a parent death to the
/// child, and it gets the signal it asked for then, not at the process's end.
#[test]
fn the_forking_thread_leaving_tells_the_child() {
    use crate::on_a_thread::on_a_thread;
    use alloc::boxed::Box;
    use linux_object::process::LinuxProcess;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::ROOT_JOB;

    let parent = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        45_466,
        "padre",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let _stays = Thread::create_linux(&parent).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let child_thread = Thread::create_linux(&child).unwrap();
    child.linux().set_pdeathsig(LinuxSignal::SIGUSR1 as u8);
    let forked = child.clone();
    on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            forked.linux().set_forking_thread(sys.thread.id());
            sys.thread.exit_linux(0);
        })
    });
    let told = child_thread
        .lock_linux()
        .signals
        .contains(LinuxSignal::SIGUSR1);
    child.exit(0);
    parent.exit(0);
    assert!(
        told,
        "the thread that forked it left and the child was never told"
    );
}
