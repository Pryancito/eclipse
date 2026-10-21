//! `kcmp(2)`: whether two tasks share a kernel resource.
//!
//! It answered `ENOSYS`. Mesa asks it whether two DRM fds are one open file
//! (`os_same_file_description`), and told nothing it warns and takes them
//! for two; systemd and CRIU ask it the same of other fds and resources.
//!
//! The order is Linux's (`kernel/kcmp.c`): both tasks, by pid or by thread
//! id (`ESRCH`), then whether the caller may look at each of them, which is
//! `ptrace_may_access(task, PTRACE_MODE_READ_REALCREDS)` (`EPERM`), then the
//! type (`EINVAL`), then what that type compares.
//!
//! The answer is 0 for one resource and 1 or 2 for two, by an order that
//! holds for the whole boot but says nothing about where either one lives:
//! `kcmp_ptr` compares the pointers after mixing each with two random
//! cookies of its type. A resource a task does not have is a null pointer,
//! so two tasks that both lack one share it. A zombie has no files, address
//! space, filesystem context, I/O context or undo list any more, but keeps
//! its signal handlers until it is reaped.

use super::*;
use core::mem::size_of;
use linux_object::fs::Epoll;
use linux_object::process::{find_task, find_thread, LinuxProcess};
use zircon_object::task::{Process, Status, Thread};

const KCMP_FILE: u32 = 0;
const KCMP_VM: u32 = 1;
const KCMP_FILES: u32 = 2;
const KCMP_FS: u32 = 3;
const KCMP_SIGHAND: u32 = 4;
const KCMP_IO: u32 = 5;
const KCMP_SYSVSEM: u32 = 6;
const KCMP_EPOLL_TFD: u32 = 7;
const KCMP_TYPES: usize = 8;

/// `struct kcmp_epoll_slot`: which entry of which epoll `KCMP_EPOLL_TFD`
/// compares with.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KcmpEpollSlot {
    /// The epoll's fd, in the second task.
    pub efd: u32,
    /// The fd the file was added under.
    pub tfd: u32,
    /// Which of the entries added under `tfd`, from 0.
    pub toff: u32,
}

lazy_static::lazy_static! {
    /// `cookies`: for each type, a word each pointer is XORed with and an
    /// odd one, top bit set, it is then multiplied by. Odd makes the product
    /// one-to-one, so the order is a total one.
    static ref COOKIES: [[usize; 2]; KCMP_TYPES] = {
        let mut cookies = [[0; 2]; KCMP_TYPES];
        for word in cookies.iter_mut().flatten() {
            let mut bytes = [0u8; size_of::<usize>()];
            kernel_hal::rand::fill_random(&mut bytes);
            *word = usize::from_ne_bytes(bytes);
        }
        for cookie in cookies.iter_mut() {
            cookie[1] |= !(usize::MAX >> 1) | 1;
        }
        cookies
    };
}

/// One side of a comparison: the process, and the thread the id named,
/// which for a pid is its first one (none, for a zombie).
struct Task {
    process: Arc<Process>,
    thread: Option<Arc<Thread>>,
}

impl Syscall<'_> {
    /// `kcmp(2)`: compare a resource of task `pid1` with one of task `pid2`.
    pub fn sys_kcmp(
        &self,
        pid1: usize,
        pid2: usize,
        kind: usize,
        idx1: usize,
        idx2: usize,
    ) -> SysResult {
        let task1 = task(pid1)?;
        let task2 = task(pid2)?;
        self.may_read(&task1)?;
        self.may_read(&task2)?;
        let kind = kind as u32;
        let (a, b) = match kind {
            KCMP_FILE => match (file(&task1, idx1), file(&task2, idx2)) {
                (Some(a), Some(b)) => (address(&a), address(&b)),
                _ => return Err(LxError::EBADF),
            },
            KCMP_VM => (vm(&task1), vm(&task2)),
            KCMP_FILES | KCMP_FS => (while_alive(&task1), while_alive(&task2)),
            KCMP_SIGHAND => (linux(&task1), linux(&task2)),
            KCMP_IO => (io(&task1), io(&task2)),
            KCMP_SYSVSEM => (undo_list(&task1), undo_list(&task2)),
            KCMP_EPOLL_TFD => return epoll_target(&task1, &task2, idx1, idx2.into()),
            _ => return Err(LxError::EINVAL),
        };
        Ok(order(a, b, kind as usize))
    }

    /// `ptrace_may_access(task, PTRACE_MODE_READ_REALCREDS)`. Reading and
    /// attaching ask the same of the ids and of `dumpable`; what tells them
    /// apart is Yama, which only judges an attach and which this kernel does
    /// not have.
    fn may_read(&self, task: &Task) -> LxResult<()> {
        let target = task.process.try_linux().ok_or(LxError::ESRCH)?;
        if LinuxProcess::may_attach_to(
            &self.linux_process().credentials(),
            &target.credentials(),
            task.process.id() == self.zircon_process().id(),
            target.dumpable(),
        ) {
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }
}

/// `find_task_by_vpid`: a pid, a zombie's included, or a thread id.
fn task(raw: usize) -> LxResult<Task> {
    let id = crate::intarg::task_pid(raw)? as KoID;
    if let Some(process) = find_task(id) {
        let thread = process.threads().first().cloned();
        return Ok(Task { process, thread });
    }
    let (process, thread) = find_thread(id).ok_or(LxError::ESRCH)?;
    Ok(Task {
        process,
        thread: Some(thread),
    })
}

fn alive(task: &Task) -> bool {
    !matches!(task.process.status(), Status::Exited(_))
}

/// The address of the task's `LinuxProcess`, which holds its fd table, its
/// filesystem context and its signal handlers, all three shared by its
/// threads and copied for a child.
fn linux(task: &Task) -> usize {
    task.process
        .try_linux()
        .map_or(0, |linux| linux as *const LinuxProcess as usize)
}

/// [`linux`] while the task lives: `exit_files` and `exit_fs` leave a
/// zombie without either.
fn while_alive(task: &Task) -> usize {
    if alive(task) {
        linux(task)
    } else {
        0
    }
}

/// The address space, which a `vfork` child shares with its parent until
/// its `execve`; a zombie has none (`exit_mm`).
fn vm(task: &Task) -> usize {
    if alive(task) {
        Arc::as_ptr(&task.process.vmar()) as usize
    } else {
        0
    }
}

/// The I/O context, which is the thread's own. Linux makes one when the
/// task's I/O priority is set, and a new one for each child or thread made
/// by a task that has one; its other way, an I/O scheduler such as BFQ
/// asking for one, does not exist here. So a priority still at its default
/// is no context. One set back to 0 (`IOPRIO_CLASS_NONE`) cannot be told
/// from one never set, where Linux would still have made a context for it.
fn io(task: &Task) -> usize {
    match &task.thread {
        Some(thread) if thread.lock_linux().ioprio != 0 => Arc::as_ptr(thread) as usize,
        _ => 0,
    }
}

/// The `SEM_UNDO` list, made by the task's first `semop` with `SEM_UNDO`
/// and kept until it exits, when it goes with the rest of the process's
/// System V state (`exit_sem`). Shared by its threads, and not by a child.
fn undo_list(task: &Task) -> usize {
    let made = task
        .process
        .try_linux()
        .is_some_and(|linux| linux.semaphores_undo_list());
    if made {
        linux(task)
    } else {
        0
    }
}

/// The open file at `idx` in the task's table, which a zombie no longer
/// has: its exit takes the table (`exit_files`). The index is an `unsigned
/// int`, so the bits above 32 are not looked at.
fn file(task: &Task, idx: usize) -> Option<Arc<dyn FileLike>> {
    task.process
        .try_linux()?
        .get_file_like(FileDesc::from(idx))
        .ok()
}

fn address(file: &Arc<dyn FileLike>) -> usize {
    Arc::as_ptr(file) as *const u8 as usize
}

/// `KCMP_EPOLL_TFD`: the first task's file at `idx1` against the file the
/// second task's epoll watches under the slot's fd, in Linux's order: the
/// slot (`EFAULT`), the first file (`EBADF`), the epoll's fd (`EBADF`),
/// whether it is an epoll (`EINVAL`), the entry (`ENOENT`).
fn epoll_target(
    task1: &Task,
    task2: &Task,
    idx1: usize,
    slot: UserInPtr<KcmpEpollSlot>,
) -> SysResult {
    let slot = slot.read()?;
    let first = file(task1, idx1).ok_or(LxError::EBADF)?;
    let epoll = file(task2, slot.efd as usize).ok_or(LxError::EBADF)?;
    let epoll = epoll.downcast_arc::<Epoll>().map_err(|_| LxError::EINVAL)?;
    let target = epoll
        .watched(FileDesc::from(slot.tfd as i32), slot.toff)
        .ok_or(LxError::ENOENT)?;
    Ok(order(address(&first), address(&target), KCMP_FILE as usize))
}

/// `kcmp_ptr`: 0 when `a` and `b` are one resource, else 1 when `a` comes
/// first and 2 when `b` does, after both are mixed with the cookies of
/// `kind`.
fn order(a: usize, b: usize, kind: usize) -> usize {
    let [xor, mul] = COOKIES[kind];
    let a = (a ^ xor).wrapping_mul(mul) as isize;
    let b = (b ^ xor).wrapping_mul(mul) as isize;
    usize::from(a < b) | usize::from(a > b) << 1
}

#[cfg(test)]
mod tests;
