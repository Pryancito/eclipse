//! `F_SETOWN` / `F_GETOWN`: who an open file description says is its owner.
//!
//! Linux keeps the owner in `struct file` (`f_owner`): it is per open file
//! description, so a `fork` or a `dup` shares it. The owner is who `SIGIO`
//! and `SIGURG` go to when `O_ASYNC` is on. Eclipse sends neither yet, so
//! this keeps the owner and gives it back, which is what a program that
//! sets one and never relies on the signal needs.
//!
//! nginx is one: its master gives each worker a channel (a socketpair)
//! and does `FIOASYNC` and then `F_SETOWN` on it, and a failed `F_SETOWN`
//! is an `alert` that closes the channel and gives up on the worker. Before
//! this the command was `EINVAL`, and nginx started with no worker at all.
//!
//! The table holds a weak reference to each description, so it keeps no
//! file open; a description that is gone is pruned on the next `F_SETOWN`.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use linux_object::error::{LxError, LxResult};
use linux_object::fs::FileLike;
use lock::Mutex;

/// `F_SETOWN`.
pub(super) const F_SETOWN: usize = 8;
/// `F_GETOWN`.
pub(super) const F_GETOWN: usize = 9;

/// `fcntl`'s owner commands: `None` when `cmd` is neither, so the caller
/// goes on with its own.
pub(super) fn fcntl(file: &Arc<dyn FileLike>, cmd: usize, arg: usize) -> Option<LxResult<usize>> {
    match cmd {
        F_GETOWN => Some(Ok(owner(file) as isize as usize)),
        F_SETOWN => Some(
            set_owner(file, arg as i32, |id| {
                linux_object::process::find_task(id).is_some()
                    || linux_object::process::find_thread(id).is_some()
            })
            .map(|_| 0),
        ),
        _ => None,
    }
}

/// The owners set, by description: a pid, or minus a process group.
static OWNERS: Mutex<Vec<(Weak<dyn FileLike>, i32)>> = Mutex::new(Vec::new());

fn same(entry: &Weak<dyn FileLike>, file: &Arc<dyn FileLike>) -> bool {
    // Only the data pointer: two `dyn` pointers to one object can carry
    // different vtables.
    core::ptr::addr_eq(entry.as_ptr(), Arc::as_ptr(file))
}

/// The owner `file` was given, 0 for none (`F_GETOWN`).
pub(super) fn owner(file: &Arc<dyn FileLike>) -> i32 {
    OWNERS
        .lock()
        .iter()
        .find(|(entry, _)| entry.strong_count() > 0 && same(entry, file))
        .map_or(0, |&(_, who)| who)
}

/// Gives `file` the owner `who` (`F_SETOWN`): a pid when positive, a
/// process group when negative, nobody when 0. `exists` says whether a pid
/// names a task, which is `f_setown`'s `find_vpid`: one that names nobody
/// is `ESRCH`, and `INT_MIN`, which has no positive, is `EINVAL`.
pub(super) fn set_owner(
    file: &Arc<dyn FileLike>,
    who: i32,
    exists: impl Fn(u64) -> bool,
) -> LxResult<()> {
    if who == i32::MIN {
        return Err(LxError::EINVAL);
    }
    if who != 0 && !exists(who.unsigned_abs() as u64) {
        return Err(LxError::ESRCH);
    }
    let mut owners = OWNERS.lock();
    owners.retain(|(entry, _)| entry.strong_count() > 0 && !same(entry, file));
    if who != 0 {
        owners.push((Arc::downgrade(file), who));
    }
    Ok(())
}
