use super::*;
use alloc::vec::Vec;
use bitflags::*;
use numeric_enum_macro::numeric_enum;
use zircon_object::vm::*;

pub use linux_object::ipc::*;

/// `shmat(2)` attach flags (`linux/shm.h`).
const SHM_RDONLY: usize = 0o10000;
const SHM_RND: usize = 0o20000;

/// `SHMLBA`: the boundary `SHM_RND` rounds a requested attach address down to.
/// Every architecture this kernel targets (x86-64, aarch64, riscv64) defines
/// `SHMLBA` as `PAGE_SIZE`; the arches where it is larger (to dodge D-cache
/// aliasing) are ones this kernel does not build for.
const SHMLBA: usize = PAGE_SIZE;

/// The access `semctl(2)` needs for `cmd`, or `None` when the command is not
/// `ipcperms`'s question at all.
///
/// `IPC_SET` and `IPC_RMID` are the two that are not: they ask who may
/// *change* the set, which is `may_control`, and they answer `EPERM`. The
/// rest split the way Linux splits them -- reading a value or the `semid_ds`
/// needs read, writing one needs write -- and answer `EACCES`.
fn semctl_access(cmd: &SemctlCmds) -> Option<u32> {
    match cmd {
        SemctlCmds::IPC_STAT
        | SemctlCmds::GETPID
        | SemctlCmds::GETVAL
        | SemctlCmds::GETALL
        | SemctlCmds::GETNCNT
        | SemctlCmds::GETZCNT => Some(IPC_R),
        SemctlCmds::SETVAL | SemctlCmds::SETALL => Some(IPC_W),
        SemctlCmds::IPC_RMID | SemctlCmds::IPC_SET => None,
    }
}

/// The access `shmat(2)` needs: read always, write unless the caller asked
/// for `SHM_RDONLY` (`do_shmat`: `S_IRUGO | (shmflg & SHM_RDONLY ? 0 :
/// S_IWUGO)`).
fn shmat_access(shmflg: usize) -> u32 {
    if shmflg & SHM_RDONLY != 0 {
        IPC_R
    } else {
        IPC_R | IPC_W
    }
}

/// Where `shmat` puts the segment.
#[derive(Debug, PartialEq, Eq)]
enum ShmatPlace {
    /// `addr == 0`: let the kernel choose the address.
    Anywhere,
    /// A specific address the caller asked for (already rounded when
    /// `SHM_RND` was set).
    At(VirtAddr),
}

/// Decide the mapping permissions and placement for one
/// `shmat(id, addr, shmflg)`.
///
/// Pure, so the whole of shmat's flag handling is unit-testable without an
/// address space -- and it needed testing, because the old code answered
/// every attach with one fixed set of flags and ignored `addr` outright.
///
/// `USER` is never optional. `VmMapping::handle_page_fault` requires the
/// page's recorded flags to contain the whole fault mask, and a ring-3 access
/// always faults with `USER` set -- so a mapping recorded without it answers
/// the very first store with `ACCESS_DENIED`, and the process takes
/// `SIGSEGV`. `sys_mmap` has always included it; this path did not, and nobody
/// noticed for as long as the segment `shmat` attached was the wrong one
/// anyway (shm ids were per-process until #1318). The first build where
/// MIT-SHM attached the right segment was the first build where an X client
/// got as far as writing into it:
///
///     unhandled page fault @ 0x11937000(WRITE | USER)
///         [anon+0x0 in map 0x11937000-0x1199b000]: ACCESS_DENIED,
///         proc=glxgears pc=... [libgallium.so+0x7e0859] -> SIGSEGV
///
/// `rep stos` zeroing the freshly attached 400 KiB image, from `glxgears`
/// and `eglgears_x11` alike, at the first byte.
///
/// `SHM_RDONLY` is the correctness fix this function exists for: an attach
/// that asks for read-only used to be handed a writable mapping, so a program
/// that attached a segment read-only and then wrote to it succeeded silently
/// where it must take `SIGSEGV`. `EXECUTE` is kept unconditionally, as the old
/// fixed flags had it -- narrowing it to `SHM_EXEC` is a separate change with
/// its own W^X risk, out of scope here.
fn shmat_flags_and_place(shmflg: usize, addr: VirtAddr) -> Result<(MMUFlags, ShmatPlace), LxError> {
    let mut flags = MMUFlags::READ | MMUFlags::EXECUTE | MMUFlags::USER;
    if shmflg & SHM_RDONLY == 0 {
        flags |= MMUFlags::WRITE;
    }

    let place = if addr == 0 {
        // shmat(2): "If shmaddr is NULL, the system chooses a suitable
        // (unused) page-aligned address." This is the case MIT-SHM uses.
        ShmatPlace::Anywhere
    } else if shmflg & SHM_RND != 0 {
        // "the attach occurs at the address rounded down to the nearest
        // multiple of SHMLBA."
        ShmatPlace::At(addr & !(SHMLBA - 1))
    } else if addr.is_multiple_of(SHMLBA) {
        ShmatPlace::At(addr)
    } else {
        // A non-aligned address without SHM_RND is EINVAL -- not, as before,
        // silently mapped somewhere else.
        return Err(LxError::EINVAL);
    };
    Ok((flags, place))
}

/// Syscalls of inter-process communication and System V semaphore Set operation.
///
/// # Menu
///
/// - [`semget`](Self::sys_semget)
/// - [`semop`](Self::sys_semop)
/// - [`semctl`](Self::sys_semctl)
/// - [`shmget`](Self::sys_shmget)
/// - [`shmat`](Self::sys_shmat)
/// - [`shmdt`](Self::sys_shmdt)
/// - [`shmctl`](Self::sys_shmctl)
impl Syscall<'_> {
    /// Get a System V semaphore set identifier
    /// (see [linux man semget(2)](https://www.man7.org/linux/man-pages/man2/semget.2.html)).
    ///
    /// The `sys_semget` system call returns
    /// the System V semaphore set identifier associated with the argument `key`.
    /// It may be used either to obtain the identifier of a previously created semaphore set
    /// (when `flags` is zero and `key` is not zero),
    /// or to create a new set.
    ///
    /// A new set of `nsems` (number of semaphores) semaphores is created if `key` is zero
    /// or if no existing semaphore set is associated with `key` and `IpcGetFlag::CREAT` is specified in `semflg`.
    ///
    /// If `flags` specifies both `IpcGetFlag::CREAT` and `IpcGetFlag::EXCLUSIVE`
    /// and a semaphore set already exists for key, then `sys_semget` fails with [`EEXIST`](LxError::EEXIST).
    /// (This is analogous to the effect of the combination `OpenFlags::CREATE | OpenFlags::EXCLUSIVE` for [`sys_open`](Self::sys_open).)
    ///
    /// Upon creation, the least significant 9 bits of the argument `flags` define
    /// the permissions (for owner, group, and others) for the semaphore set.
    /// These bits have the same format, and the same meaning, as the `mode` argument of [`sys_open`](Self::sys_open)
    /// (though the execute permissions are not meaningful for semaphores,
    /// and write permissions mean permission to alter semaphore values).
    ///
    /// When creating a new semaphore set, `sys_semget` initializes the set's associated data structure,
    /// semid_ds (see [`sys_semctl`](Self::sys_semctl)), as follows:
    ///
    /// - sem_perm.cuid and sem_perm.uid are set to the effective user ID of the calling process.
    /// - sem_perm.cgid and sem_perm.gid are set to the effective group ID of the calling process.
    /// - The least significant 9 bits of sem_perm.mode are set to the least significant 9 bits of `flags`.
    /// - sem_nsems is set to the value of nsems.
    /// - sem_otime is set to 0.
    /// - sem_ctime is set to the current time.
    ///
    /// The argument nsems can be 0 (a don't care) when a semaphore set is not being created.
    /// Otherwise, nsems must be greater than 0 and
    /// less than or equal to the maximum number of semaphores per semaphore set (SEMMSL, constant 256).
    ///
    /// If the semaphore set already exists, the permissions are verified.
    pub fn sys_semget(&self, key: usize, nsems: usize, flags: usize) -> SysResult {
        info!("semget: key: {} nsems: {} flags: {:#x}", key, nsems, flags);

        /// The maximum semaphores per semaphore set
        const SEMMSL: usize = 256;

        if nsems > SEMMSL {
            return Err(LxError::EINVAL);
        }

        let proc = self.linux_process();
        let sem_array =
            SemArray::get_or_create(key as u32, nsems, flags, proc.euid(), proc.egid())?;
        let id = self.linux_process().semaphores_add(sem_array);
        Ok(id)
    }

    /// System V semaphore operations
    /// (see [linux man semop(2)](https://www.man7.org/linux/man-pages/man2/semop.2.html)).
    ///
    /// `semop` performs operations on selected semaphores in the set indicated by `id`.
    /// An array `[SemBuf; num_ops]` pointed to by `ops` specifies an operation to be performed on a single semaphore.
    /// The declaration of `SemBuf` is like this:
    ///
    /// ```rust
    /// struct SemBuf {
    ///    num: u16,
    ///    op: i16,
    ///    flags: i16,
    /// }
    /// ```
    ///
    /// Flags recognized in `SemBuf::flags` are `SemFlags::IPC_NOWAIT` and `SemFlags::SEM_UNDO`.
    /// If an operation specifies `SEM_UNDO`, it will be automatically undone when the process terminates.
    ///
    /// Each operation is performed on the `SemBuf::num`-th semaphore of the semaphore set,
    /// where the first semaphore of the set is numbered 0.
    /// There are two types of operation, distinguished by the value of `SemBuf::op`.
    ///
    /// - If `op` is +1, see [`acquire`](linux_object::sync::Semaphore::acquire).
    /// - If `op` is -1, see [`release`](linux_object::sync::Semaphore::release).
    pub async fn sys_semop(&self, id: usize, ops: UserInPtr<SemBuf>, num_ops: usize) -> SysResult {
        info!("semop: id: {}", id);
        // semop(2): EINVAL for an empty array, E2BIG for one over SEMOPM.
        // Checked before reading the pointer, as Linux does.
        if num_ops == 0 {
            return Err(LxError::EINVAL);
        }
        if num_ops > SEMOPM {
            return Err(LxError::E2BIG);
        }
        let ops = ops.as_slice(num_ops)?;

        let sem_array = self
            .linux_process()
            .semaphores_get(id)
            .ok_or(LxError::EINVAL)?;
        // An operation that changes a semaphore needs write; one that only
        // waits for zero needs read (`semop` -> `ipcperms`). Nothing asked
        // before, so naming the id was the whole check.
        let alter = ops.iter().any(|op| op.op != 0);
        let proc = self.linux_process();
        if !sem_array.may_access(proc.euid(), proc.egid(), if alter { IPC_W } else { IPC_R }) {
            return Err(LxError::EACCES);
        }
        sem_array.otime();
        let pid = self.zircon_process().id() as usize;

        loop {
            // Plan and apply under the set-wide lock, so the whole array lands
            // as one unit or not at all -- semop(2)'s central promise, and the
            // one the old op-at-a-time loop broke: it applied each operation as
            // it went, so a `[+1, -2]` that had to block left the `+1` visible
            // to every other process and then waited forever holding it.
            let blocked_on = {
                let _guard = sem_array.semop_guard();
                let snapshot = sem_array.snapshot();
                let values: Vec<isize> = snapshot.iter().map(|&(v, _)| v).collect();
                match plan_semop(&values, ops)? {
                    SemopPlan::Apply(new_values) => {
                        for (idx, value) in new_values {
                            // Every index came out of `plan_semop`, which
                            // bounds-checked it against this same snapshot.
                            if let Some(sem) = sem_array.get_sem(idx) {
                                sem.set(value);
                                sem.set_pid(pid);
                            }
                        }
                        for &SemBuf { num, op, flags } in ops {
                            if SemFlags::from_bits_truncate(flags).contains(SemFlags::SEM_UNDO) {
                                self.linux_process().semaphores_add_undo(id, num, op);
                            }
                        }
                        return Ok(0);
                    }
                    // Nothing was applied. Remember which semaphore is in
                    // the way, at what generation (so the wait below cannot
                    // miss a change that lands in between), and whether the
                    // blocking operation asked not to wait.
                    SemopPlan::WouldBlock { sem_num, op_index } => {
                        let nowait = SemFlags::from_bits_truncate(ops[op_index].flags)
                            .contains(SemFlags::IPC_NOWAIT);
                        (sem_num, snapshot[sem_num].1, nowait)
                    }
                }
            };

            let (sem_num, generation, nowait) = blocked_on;
            if nowait {
                // semop(2): with IPC_NOWAIT the call "fails with errno set to
                // EAGAIN" rather than blocking. It used to answer ENOSYS,
                // which is not a "try again later" at all -- a caller probing
                // a semaphore without blocking read it as "this kernel has no
                // semaphores" and gave up for good.
                return Err(LxError::EAGAIN);
            }
            sem_array
                .get_sem(sem_num)
                .ok_or(LxError::EFBIG)?
                .wait_for_change(generation)
                .await?;
        }
    }

    /// System V semaphore control operations
    /// (see [linux man semctl(2)](https://www.man7.org/linux/man-pages/man2/semctl.2.html)).
    ///
    /// `semctl` performs the control operation specified by cmd
    /// on the System V semaphore set identified by `id`,
    /// or on the `num`-th semaphore of that set
    /// (The semaphores in a set are numbered starting at 0).
    ///
    /// TODO
    pub fn sys_semctl(&self, id: usize, num: usize, cmd: usize, arg: usize) -> SysResult {
        info!(
            "semctl: id: {}, num: {}, cmd: {} arg: {:#x}",
            id, num, cmd, arg
        );
        let sem_array = self
            .linux_process()
            .semaphores_get(id)
            .ok_or(LxError::EINVAL)?;

        let cmd = match SemctlCmds::try_from(cmd) {
            Ok(t) => t,
            Err(_) => {
                error!("invalid semctl cmd: {}", cmd);
                return Err(LxError::EINVAL);
            }
        };
        if let Some(want) = semctl_access(&cmd) {
            let proc = self.linux_process();
            if !sem_array.may_access(proc.euid(), proc.egid(), want) {
                return Err(LxError::EACCES);
            }
        }
        match cmd {
            SemctlCmds::IPC_RMID => {
                if !sem_array.may_control(self.linux_process().euid()) {
                    return Err(LxError::EPERM);
                }
                sem_array.remove();
                self.linux_process().semaphores_remove(id);
                Ok(0)
            }
            SemctlCmds::IPC_SET => {
                // arg is struct semid_ds
                let ptr = UserInPtr::from(arg);
                let ds: SemidDs = ptr.read()?;
                // update IpcPerm
                sem_array.set(&ds, self.linux_process().euid())?;
                sem_array.ctime();
                Ok(0)
            }
            SemctlCmds::IPC_STAT => {
                // arg is struct semid_ds
                let mut ptr = UserOutPtr::from(arg);
                ptr.write(*sem_array.semid_ds.lock())?;
                Ok(0)
            }
            _ => {
                // `num` comes from userspace. This used to index the set
                // directly, so `semctl(id, 9999, GETVAL)` from any process was
                // a kernel panic; semctl(2) says EINVAL. Its sibling `semop`
                // had the check all along — see `SemArray::get_sem`.
                let sem = sem_array.get_sem(num).ok_or(LxError::EINVAL)?;
                match cmd {
                    SemctlCmds::GETPID => Ok(sem.get_pid()),
                    SemctlCmds::GETVAL => Ok(sem.get() as usize),
                    SemctlCmds::GETNCNT => Ok(sem.get_ncnt()),
                    SemctlCmds::GETZCNT => Ok(0),
                    SemctlCmds::SETVAL => {
                        sem.set(setval_from_arg(arg)?);
                        sem.set_pid(self.zircon_process().id() as usize);
                        sem_array.ctime();
                        Ok(0)
                    }
                    _ => {
                        warn!("unsupported semctl cmd: {:?}", cmd);
                        Err(LxError::EINVAL)
                    }
                }
            }
        }
    }

    /// Get a System V message queue identifier
    /// (see [linux man msgget(2)](https://www.man7.org/linux/man-pages/man2/msgget.2.html)).
    ///
    /// Queue ids are global and the queue persists until `IPC_RMID`, per
    /// sysvipc(7); `key == 0` is `IPC_PRIVATE`.
    pub fn sys_msgget(&self, key: usize, msgflg: usize) -> SysResult {
        info!("msgget: key={}, flags={:#x}", key, msgflg);
        let proc = self.linux_process();
        msg_get(key as u32, msgflg, proc.euid(), proc.egid())
    }

    /// Append a message to a System V queue
    /// (see [linux man msgsnd(2)](https://www.man7.org/linux/man-pages/man2/msgsnd.2.html)).
    ///
    /// `msgp` points at `struct msgbuf { long mtype; char mtext[]; }`. A full
    /// queue blocks the caller (interruptibly) unless `IPC_NOWAIT` asks for
    /// `EAGAIN`; a queue removed mid-wait answers `EIDRM`.
    pub async fn sys_msgsnd(
        &self,
        id: usize,
        msgp: usize,
        msgsz: usize,
        msgflg: usize,
    ) -> SysResult {
        info!(
            "msgsnd: id={}, msgp={:#x}, msgsz={}, flags={:#x}",
            id, msgp, msgsz, msgflg
        );
        if msgsz > MSGMAX {
            return Err(LxError::EINVAL);
        }
        let mtype: isize = UserInPtr::<isize>::from(msgp).read()?;
        // msgsnd(2): the type must be strictly positive.
        if mtype < 1 {
            return Err(LxError::EINVAL);
        }
        let data = UserInPtr::<u8>::from(msgp + core::mem::size_of::<isize>()).read_array(msgsz)?;
        let queue = msg_queue(id).ok_or(LxError::EINVAL)?;
        let proc = self.linux_process();
        if !queue.may_access(proc.euid(), proc.egid(), IPC_W) {
            return Err(LxError::EACCES);
        }
        let sender = self.zircon_process().id() as u32;
        loop {
            // A full queue parks the caller on the queue itself: `try_send`
            // hands back the generation it looked at, and `wait_for_change`
            // returns when a receive makes room, when `IPC_RMID` runs
            // (`EIDRM`) or when a signal arrives (`EINTR`). This used to be a
            // 5 ms sleep-and-look-again.
            let since = match queue.try_send(mtype, &data, sender) {
                Ok(()) => return Ok(0),
                Err(MsgSendError::Removed) => return Err(LxError::EIDRM),
                Err(MsgSendError::Full(since)) => {
                    if msgflg & IPC_NOWAIT != 0 {
                        return Err(LxError::EAGAIN);
                    }
                    since
                }
            };
            queue.wait_for_change(since).await?;
        }
    }

    /// Take a message from a System V queue
    /// (see [linux man msgrcv(2)](https://www.man7.org/linux/man-pages/man2/msgrcv.2.html)).
    ///
    /// `msgtyp` selects the message (0 = first; >0 = that type, inverted by
    /// `MSG_EXCEPT`; <0 = lowest type ≤ |msgtyp|); `MSG_NOERROR` truncates an
    /// oversized message instead of failing with `E2BIG`. Returns the number
    /// of payload bytes copied.
    pub async fn sys_msgrcv(
        &self,
        id: usize,
        msgp: usize,
        msgsz: usize,
        msgtyp: isize,
        msgflg: usize,
    ) -> SysResult {
        info!(
            "msgrcv: id={}, msgp={:#x}, msgsz={}, msgtyp={}, flags={:#x}",
            id, msgp, msgsz, msgtyp, msgflg
        );
        let queue = msg_queue(id).ok_or(LxError::EINVAL)?;
        let proc = self.linux_process();
        if !queue.may_access(proc.euid(), proc.egid(), IPC_R) {
            return Err(LxError::EACCES);
        }
        let receiver = self.zircon_process().id() as u32;
        let noerror = msgflg & MSG_NOERROR != 0;
        let except = msgflg & MSG_EXCEPT != 0;
        loop {
            // Same shape as `msgsnd`: park on the queue's own generation
            // until a message lands, instead of a 5 ms sleep-and-look-again.
            let since = match queue.try_recv(msgtyp, msgsz, noerror, except, receiver) {
                Ok((mtype, data)) => {
                    UserOutPtr::<isize>::from(msgp).write(mtype)?;
                    UserOutPtr::<u8>::from(msgp + core::mem::size_of::<isize>())
                        .write_array(&data)?;
                    return Ok(data.len());
                }
                Err(MsgRecvError::Removed) => return Err(LxError::EIDRM),
                Err(MsgRecvError::TooBig) => return Err(LxError::E2BIG),
                Err(MsgRecvError::NoMsg(since)) => {
                    if msgflg & IPC_NOWAIT != 0 {
                        return Err(LxError::ENOMSG);
                    }
                    since
                }
            };
            queue.wait_for_change(since).await?;
        }
    }

    /// System V message queue control operations
    /// (see [linux man msgctl(2)](https://www.man7.org/linux/man-pages/man2/msgctl.2.html)).
    pub fn sys_msgctl(&self, id: usize, cmd: usize, buf: usize) -> SysResult {
        info!("msgctl: id={}, cmd={}, buf={:#x}", id, cmd, buf);
        const IPC_RMID: usize = 0;
        const IPC_SET: usize = 1;
        const IPC_STAT: usize = 2;
        match cmd {
            IPC_RMID => msg_remove(id, self.linux_process().euid()).map(|_| 0),
            IPC_SET => {
                let queue = msg_queue(id).ok_or(LxError::EINVAL)?;
                let ds: MsqidDs = UserInPtr::from(buf).read()?;
                queue.set(&ds, self.linux_process().euid())?;
                Ok(0)
            }
            IPC_STAT => {
                let queue = msg_queue(id).ok_or(LxError::EINVAL)?;
                let proc = self.linux_process();
                if !queue.may_access(proc.euid(), proc.egid(), IPC_R) {
                    return Err(LxError::EACCES);
                }
                UserOutPtr::from(buf).write(queue.stat())?;
                Ok(0)
            }
            _ => {
                warn!("msgctl: unsupported cmd {}", cmd);
                Err(LxError::EINVAL)
            }
        }
    }

    /// Allocates a System V shared memory segment
    /// (see [linux man shmget(2)](https://www.man7.org/linux/man-pages/man2/shmget.2.html)).
    ///
    /// `shmget` returns the identifier of the System V shared memory segment
    /// associated with the value of the argument key.
    /// Differ from linux, this syscall always create a new set.
    pub fn sys_shmget(&self, key: usize, size: usize, shmflg: usize) -> SysResult {
        info!(
            "shmget: key: {}, size: {}, shmflg: {:#x}",
            key, size, shmflg
        );

        let proc = self.linux_process();
        let shared_guard = ShmIdentifier::new_shared_guard(
            key as u32,
            size,
            shmflg,
            self.zircon_process().id() as u32,
            proc.euid(),
            proc.egid(),
        )?;
        // The id is system-wide: `shmget` hands out a number that names the
        // same segment in every process, because passing it to another
        // program is the whole mechanism. See `shm_register`.
        let id = linux_object::ipc::shm_register(&shared_guard)?;
        self.linux_process().shm_add(id, shared_guard);
        Ok(id)
    }

    /// System V shared memory operations
    /// (see [linux man shmat(2)](https://www.man7.org/linux/man-pages/man2/shmat.2.html)).
    ///
    /// `shmat` attaches the System V shared memory segment identified by `id`
    /// to the address space of the calling process.
    /// The attaching address is specified by `addr`.
    /// If `addr` is zero, the system chooses a suitable page-aligned address to attach the segment.
    pub fn sys_shmat(&self, id: usize, addr: VirtAddr, shmflg: usize) -> SysResult {
        // mmap_lock (LinuxProcess::aspace_lock): shmat maps into the address
        // space — a layout mutation a concurrent fork must not race. Taken
        // before the `inner`-touching shm lookup (global lock order).
        let _aspace = self.linux_process().aspace_lock().lock();
        // Permissions and placement first, so a bad `addr` is rejected before
        // any lookup or mapping: read-only means read-only, a non-aligned
        // address without SHM_RND is EINVAL, and SHM_RND rounds down.
        let (flags, place) = shmat_flags_and_place(shmflg, addr)?;
        // Looked up system-wide, NOT in this process's own map: the id may
        // have been created by another program and passed here -- which is
        // what the X11 shared-memory extension does with every image.
        let guard = linux_object::ipc::shm_lookup(id).ok_or(LxError::EINVAL)?;
        {
            let proc = self.linux_process();
            if !guard
                .lock()
                .may_access(proc.euid(), proc.egid(), shmat_access(shmflg))
            {
                return Err(LxError::EACCES);
            }
        }
        let proc = self.zircon_process();
        let vmar = proc.vmar();
        let vmo = guard.lock().shared_guard.clone();
        info!(
            "shmat: id: {}, place = {:?}, size = {}, flags = {:?}",
            id,
            place,
            vmo.len(),
            flags
        );
        // A requested address is an offset into the process's root vmar, as
        // `sys_mmap` does for MAP_FIXED. `Anywhere` lets the kernel choose,
        // which is the MIT-SHM path and the only one that reached here before.
        let vmar_offset = match place {
            ShmatPlace::Anywhere => None,
            ShmatPlace::At(want) => Some(want - vmar.addr()),
        };
        let addr = vmar.map(vmar_offset, vmo.clone(), 0, vmo.len(), flags)?;
        // Account on the segment, then record where in the process -- one
        // record per attachment, so a segment attached twice can be
        // detached twice (the old table kept one address per id and lost
        // the first). Neither lock is held while taking the other: the
        // process lock is taken with segments locked under it by `fork` and
        // by the table's drop at `exit`.
        guard.lock().attach(proc.id() as u32);
        self.linux_process().shm_attach(id, guard, addr);
        Ok(addr)
    }

    /// System V shared memory operations
    /// (see [linux man shmdt(2)](https://www.man7.org/linux/man-pages/man2/shmdt.2.html)).
    ///
    /// `shmdt` detaches the shared memory segment located at the address specified by `addr`
    /// from the address space of the calling process.
    /// The to-be-detached segment must be currently attached with `addr`
    /// equal to the value returned by the attaching [`sys_shmat`](Self::sys_shmat) call.
    pub fn sys_shmdt(&self, id: usize, addr: VirtAddr, shmflg: usize) -> SysResult {
        // mmap_lock: shmdt unmaps the segment — a layout mutation (see shmat).
        let _aspace = self.linux_process().aspace_lock().lock();
        info!(
            "shmdt: id = {}, addr = {:#x}, flag = {:#x}",
            id, addr, shmflg
        );
        let proc = self.linux_process();
        // shmdt(2): an address nothing is attached at is EINVAL. It used to
        // answer 0, which also covered the second attachment of a segment
        // the old table had forgotten.
        let shm_identifier = proc.shm_detach(addr).ok_or(LxError::EINVAL)?;
        // shmat() mapped the shared VMO into this address space; shmdt() must
        // remove that mapping. Previously it only dropped the tracking entry
        // and decremented nattch, leaving the segment MAPPED after detach:
        // the region stayed writable, its shared frames stayed pinned, and a
        // later MAP_FIXED mmap or re-attach at the same VA collided with the
        // stale mapping in the address space's VMAR. Under GL=1, Mesa's DRI
        // buffers churn shmat/shmdt hard and concurrently, so that stale-
        // mapping / VMAR inconsistency is exactly the kind of state a
        // parallel munmap/teardown then trips over. Unmap first, best-effort
        // (the addr came from our own attach record), then account the
        // detach.
        let size = shm_identifier.guard.lock().shared_guard.len();
        let _ = self
            .zircon_process()
            .vmar()
            .unmap(shm_identifier.addr, size);
        shm_identifier
            .guard
            .lock()
            .detach(self.zircon_process().id() as u32);
        Ok(0)
    }

    /// System V shared memory operations
    /// (see [linux man shmctl(2)](https://www.man7.org/linux/man-pages/man2/shmctl.2.html)).
    ///
    /// performs the control operation specified by cmd on the shared memory segment whose identifier is given in id
    pub fn sys_shmctl(&self, id: usize, cmd: usize, buffer: usize) -> SysResult {
        info!("shmctl: id: {}, cmd: {} buffer: {:#x}", id, cmd, buffer);
        // System-wide, like `shmat`: a program may be asked to remove or stat
        // a segment it never created itself.
        let guard = linux_object::ipc::shm_lookup(id)
            .or_else(|| self.linux_process().shm_get(id).map(|i| i.guard))
            .ok_or(LxError::EINVAL)?;
        let shm_guard = guard.lock();
        let cmd = match ShmctlCmds::try_from(cmd) {
            Ok(t) => t,
            Err(_) => {
                error!("invalid semctl cmd: {}", cmd);
                return Err(LxError::EINVAL);
            }
        };
        match cmd {
            ShmctlCmds::IPC_RMID => {
                if !shm_guard.may_control(self.linux_process().euid()) {
                    return Err(LxError::EPERM);
                }
                shm_guard.remove();
                linux_object::ipc::shm_unregister(id);
                // The attachment stays. shmget(2): the segment is destroyed
                // only once the last process detaches, and every user of the
                // X11 extension removes the id the moment it has attached --
                // dropping the attachment here left `shmdt` with nothing to
                // find, so the mapping was never torn down and each image
                // leaked its address range for the life of the process.
                Ok(0)
            }
            ShmctlCmds::IPC_SET => {
                let buffer: UserInPtr<ShmidDs> = buffer.into();
                let set_ds = buffer.read()?;
                shm_guard.set(&set_ds, self.linux_process().euid())?;
                shm_guard.ctime();
                Ok(0)
            }
            ShmctlCmds::IPC_STAT | ShmctlCmds::SHM_STAT => {
                let proc = self.linux_process();
                if !shm_guard.may_access(proc.euid(), proc.egid(), IPC_R) {
                    return Err(LxError::EACCES);
                }
                let shmid_ds = shm_guard.shmid_ds.lock();
                let mut buffer: UserOutPtr<ShmidDs> = buffer.into();
                buffer.write(*shmid_ds)?;
                Ok(0)
            }
            ShmctlCmds::SHM_INFO => {
                let mut buffer: UserOutPtr<ShmInfo> = buffer.into();
                buffer.write(ShmInfo::default())?;
                Ok(0)
            }
            _ => {
                warn!("unsupported shmctl cmd: {:?}", cmd);
                Err(LxError::EINVAL)
            }
        }
    }
}

/// Largest value a semaphore may be set to (`SEMVMX`, include/uapi/linux/sem.h).
///
/// `semctl(SETVAL)` answers ERANGE above it, and so does a `semop` whose
/// additions would carry a semaphore past it.
const SEMVMX: i32 = 32767;

/// What `semctl(.., SETVAL, arg)` makes of its `arg`.
///
/// `arg` is the `int val` member of `union semun`, and a variadic argument
/// arrives in a whole register: reading all 64 bits of it turns the `-1` a
/// program passes by mistake into 4294967295 and sets the semaphore to it,
/// rather than answering. Linux reads the `int`, and `semctl(2)` says ERANGE
/// for anything outside `[0, SEMVMX]`.
fn setval_from_arg(arg: usize) -> Result<isize, LxError> {
    let val = arg as u32 as i32;
    if (0..=SEMVMX).contains(&val) {
        Ok(val as isize)
    } else {
        Err(LxError::ERANGE)
    }
}

numeric_enum! {
    #[repr(usize)]
    #[derive(Debug, Eq, PartialEq)]
    #[allow(non_camel_case_types)]
    /// for the third argument of semctl(), specified the control operation
    pub enum SemctlCmds {
        /// Immediately remove the semaphore set, awakening all processes blocked
        IPC_RMID = 0,
        /// Write the values of some members of the semid_ds structure pointed to by arg
        IPC_SET = 1,
        /// Copy information from the kernel data structure associated with
        /// semid into the semid_ds structure pointed to by arg.buf.
        IPC_STAT = 2,
        /// Get the value of sempid
        GETPID = 11,
        /// Get the value of semval
        GETVAL = 12,
        /// Get semval for all semaphores of the set into arg.array
        GETALL = 13,
        /// Get the value of semncnt
        GETNCNT = 14,
        /// Get the value of semzcnt
        GETZCNT = 15,
        /// Set the value of semval to arg.val
        SETVAL = 16,
        /// Set semval for all semaphores of the set using arg.array
        SETALL = 17,
    }
}

numeric_enum! {
    #[repr(usize)]
    #[derive(Debug, Eq, PartialEq)]
    #[allow(non_camel_case_types)]
    /// for the third argument of semctl(), specified the control operation
    pub enum ShmctlCmds {
        /// Mark the segment to be destroyed, actually be destroyed after the last process detaches it
        IPC_RMID = 0,
        /// Write the values of some members of the shmid_ds structure pointed to by arg
        IPC_SET = 1,
        /// Copy information from the kernel data structure associated with
        /// shmid into the shmid_ds structure pointed to by arg.buf.
        IPC_STAT = 2,
        /// Prevent swapping of the shared memory segment
        SHM_LOCK = 11,
        /// Unlock the segment, allowing it to be swapped out.
        SHM_UNLOCK = 12,
        /// Returns a shmid_ds structure as for IPC_STAT
        SHM_STAT = 13,
        /// Returns a shm_info structure whose fields contain information
        /// about system resources consumed by shared memory.
        SHM_INFO = 14,
    }
}

/// An operation to be performed on a single semaphore
///
/// Ref: <http://man7.org/linux/man-pages/man2/semop.2.html>
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemBuf {
    num: u16,
    op: i16,
    flags: i16,
}

impl SemBuf {
    /// Build one, for tests and for callers that assemble an array by hand.
    pub fn new(num: u16, op: i16, flags: i16) -> Self {
        SemBuf { num, op, flags }
    }
}

/// Linux `SEMOPM`: the most operations one `semop(2)` may carry.
const SEMOPM: usize = 500;

/// What a whole `sembuf` array does to a semaphore set, decided in one go.
#[derive(Debug, PartialEq, Eq)]
pub enum SemopPlan {
    /// Every operation can proceed. These are the new values, by index, and
    /// only for the semaphores that actually change.
    Apply(Vec<(usize, isize)>),
    /// The operation cannot proceed yet, so **nothing** is applied: the
    /// caller blocks on `sem_num`, or answers `EAGAIN` when that one
    /// operation carried `IPC_NOWAIT`.
    WouldBlock {
        /// Index of the semaphore standing in the way.
        sem_num: usize,
        /// Index, in the `sops` array, of the operation that cannot proceed.
        /// `IPC_NOWAIT` is per-operation -- semop(2) fails with `EAGAIN` only
        /// when THIS operation set it -- so the blocking op has to be named,
        /// not just the semaphore.
        op_index: usize,
    },
}

/// Decide what one `semop(2)` array does to a set, given the set's current
/// values.
///
/// Pure, and deliberately so: the atomicity rule -- "the operations are
/// performed either as a complete unit, or not at all" -- is not something a
/// loop that applies as it goes can be tested for. Here the whole array is
/// decided before a single value is written.
///
/// The three kinds of operation, from semop(2). This kernel used to implement
/// two of them, as the literal values `+1` and `-1`, and answer `ENOSYS` to
/// everything else:
///
/// * `sem_op > 0` -- add it to the value. Never blocks. A counting semaphore
///   released by more than one, or a barrier posting `n` at once, is an
///   ordinary thing to write and got `ENOSYS`.
/// * `sem_op < 0` -- wait until the value is at least `-sem_op`, then
///   subtract. Taking two units of a resource at once is one operation, and
///   it matters that it is: two `-1`s in a row can deadlock against another
///   process doing the same in the other order, which is the whole reason
///   semop takes an array.
/// * `sem_op == 0` -- **wait for zero**. Not a no-op: it is how a process
///   waits for a resource to be fully released, and it was `ENOSYS` too.
///
/// Operations inside one array are applied in order and see each other's
/// effect, so `[+1, -1]` on a semaphore at 0 succeeds while `[-1, +1]`
/// blocks.
pub fn plan_semop(values: &[isize], ops: &[SemBuf]) -> Result<SemopPlan, LxError> {
    // Worked on a copy: the caller's values must not move until every
    // operation has been shown to fit.
    let mut planned: Vec<isize> = values.to_vec();
    for (op_index, op) in ops.iter().enumerate() {
        let num = op.num as usize;
        // `num` comes from userspace. semop(2) gives EFBIG for a semaphore
        // number outside the set.
        let value = *planned.get(num).ok_or(LxError::EFBIG)?;
        let new = match op.op {
            0 => {
                if value != 0 {
                    return Ok(SemopPlan::WouldBlock {
                        sem_num: num,
                        op_index,
                    });
                }
                value
            }
            delta if delta > 0 => {
                let new = value.saturating_add(delta as isize);
                if new > SEMVMX as isize {
                    return Err(LxError::ERANGE);
                }
                new
            }
            delta => {
                // `-(i16::MIN)` overflows an i16; widen first.
                let needed = -(delta as isize);
                if value < needed {
                    return Ok(SemopPlan::WouldBlock {
                        sem_num: num,
                        op_index,
                    });
                }
                value - needed
            }
        };
        planned[num] = new;
    }
    // Only what actually moved, so an array of no-ops writes nothing and
    // bumps no generation counter.
    Ok(SemopPlan::Apply(
        planned
            .into_iter()
            .enumerate()
            .filter(|&(i, v)| v != values[i])
            .collect(),
    ))
}

/// shm_info structure for shmctl
#[repr(C)]
#[derive(Default)]
struct ShmInfo {
    /// currently existing segments
    used_ids: i32,
    /// Total number of shared memory pages
    shm_tot: usize,
    /// of resident shared memory pages
    shm_rss: usize,
    /// of swapped shared memory pages
    shm_swp: usize,
}

bitflags! {
    pub struct SemFlags: i16 {
        /// For SemOP
        const IPC_NOWAIT = 0x800;
        /// it will be automatically undone when the process terminates.
        const SEM_UNDO = 0x1000;
    }
}

/// `IPC_NOWAIT` as msgsnd/msgrcv take it: fail with EAGAIN/ENOMSG instead of
/// blocking.
const IPC_NOWAIT: usize = 0o4000;
/// msgrcv(2) `MSG_NOERROR`: truncate an oversized message instead of E2BIG.
const MSG_NOERROR: usize = 0o10000;
/// msgrcv(2) `MSG_EXCEPT`: with msgtyp > 0, take the first message of a
/// *different* type.
const MSG_EXCEPT: usize = 0o20000;

/// The System V IPC syscalls, tested where they can be: the parts that are a
/// decision about a userspace value rather than a walk through process state.
///
/// The bounds check `sys_semctl` was missing lives in
/// `linux_object::ipc::SemArray::get_sem` and is tested there; the `Index`
/// impl it used to go through is gone, so the line cannot be written again.
#[cfg(test)]
mod ipc_tests {
    use super::*;

    #[test]
    fn setval_takes_the_int_and_not_the_register() {
        assert_eq!(setval_from_arg(0), Ok(0));
        assert_eq!(setval_from_arg(1), Ok(1));
        assert_eq!(setval_from_arg(SEMVMX as usize), Ok(SEMVMX as isize));
    }

    #[test]
    fn setval_refuses_a_negative_value() {
        // `semctl(id, 0, SETVAL, -1)`: the register holds 0xffff_ffff_ffff_ffff
        // on a 64-bit caller and 0xffff_ffff sign-extended from the `int` on
        // the wire either way. Both are -1, and neither may become 4294967295.
        assert_eq!(setval_from_arg(usize::MAX), Err(LxError::ERANGE));
        assert_eq!(setval_from_arg(0xffff_ffff), Err(LxError::ERANGE));
        assert_eq!(setval_from_arg(-1i64 as usize), Err(LxError::ERANGE));
        assert_eq!(setval_from_arg(0x8000_0000), Err(LxError::ERANGE));
    }

    #[test]
    fn setval_refuses_a_value_above_semvmx() {
        assert_eq!(setval_from_arg(SEMVMX as usize + 1), Err(LxError::ERANGE));
        assert_eq!(setval_from_arg(70000), Err(LxError::ERANGE));
    }

    /// The bit that cost glxgears its first frame: a `shmat` mapping without
    /// `USER` answers the first user store with `ACCESS_DENIED`.
    #[test]
    fn a_shmat_mapping_is_accessible_from_user_mode() {
        let (flags, _) = shmat_flags_and_place(0, 0).unwrap();
        assert!(
            flags.contains(MMUFlags::USER),
            "a ring-3 store faults with USER set, and the fault handler \
             requires the recorded flags to contain the whole mask"
        );
        assert!(flags.contains(MMUFlags::READ | MMUFlags::WRITE));
    }

    #[test]
    fn setval_ignores_the_high_half_of_the_register() {
        // Whatever a variadic caller left in the top 32 bits is not part of
        // the `int`, so it must neither reach the semaphore nor cause ERANGE.
        assert_eq!(setval_from_arg(0xdead_beef_0000_0005), Ok(5));
    }
}

#[cfg(test)]
mod semop_plan_tests {
    //! `semop(2)` is not "decrement a counter". It is an array of operations
    //! applied **atomically**, in three kinds, and this kernel implemented one
    //! kind (`-1`), one other literal value (`+1`), and answered `ENOSYS` to
    //! everything else -- including `sem_op == 0`, which is how a process
    //! waits for a resource to be fully released.
    //!
    //! Worse than the missing kinds was the missing atomicity: the old loop
    //! applied each operation as it reached it, so an array that had to block
    //! halfway through left its earlier operations visible to every other
    //! process and then waited, holding them. That is a deadlock the caller
    //! cannot see and cannot escape.
    //!
    //! `plan_semop` decides the whole array before a single value is written,
    //! which is the only shape in which "all or nothing" is a testable claim.

    use super::{plan_semop, SemBuf, SemopPlan, SEMVMX};
    use crate::LxError;
    use alloc::vec::Vec;

    fn op(num: u16, delta: i16) -> SemBuf {
        SemBuf::new(num, delta, 0)
    }

    fn applied(values: &[isize], ops: &[SemBuf]) -> Vec<(usize, isize)> {
        match plan_semop(values, ops) {
            Ok(SemopPlan::Apply(changes)) => changes,
            other => panic!("expected Apply, got {:?}", other),
        }
    }

    /// The `(sem_num, op_index)` of the operation that blocks, or a panic if
    /// the array does not block.
    fn blocks(values: &[isize], ops: &[SemBuf]) -> (usize, usize) {
        match plan_semop(values, ops) {
            Ok(SemopPlan::WouldBlock { sem_num, op_index }) => (sem_num, op_index),
            other => panic!("expected WouldBlock, got {:?}", other),
        }
    }

    /// The whole array, or nothing. An operation that has to wait must leave
    /// every earlier operation in the same array unapplied -- semop(2): "the
    /// operations are performed either as a complete unit, or not at all".
    ///
    /// This is the one the old implementation got wrong and the one that
    /// deadlocks: `[+1, -2]` posted the `+1` for everyone to see and then
    /// blocked forever on a unit it had just made available to itself.
    #[test]
    fn an_array_that_blocks_applies_nothing_at_all() {
        assert_eq!(
            blocks(&[0], &[op(0, 1), op(0, -2)]),
            (0, 1),
            "the +1 must not be applied when the -2 cannot proceed"
        );
        // Across different semaphores too: the successful ones do not land.
        assert_eq!(blocks(&[5, 0], &[op(0, -5), op(1, -1)]), (1, 1));
    }

    /// `sem_op > 0` adds that many, not one. A counting semaphore released
    /// by several units at once, or a barrier posting `n`, is ordinary and
    /// used to be `ENOSYS`.
    #[test]
    fn a_positive_op_adds_its_own_value() {
        assert_eq!(applied(&[0], &[op(0, 7)]), alloc::vec![(0, 7)]);
        assert_eq!(applied(&[3], &[op(0, 1)]), alloc::vec![(0, 4)]);
    }

    /// `sem_op < 0` waits for at least that many and takes them in one go.
    /// Taking two units as one operation is not the same as two `-1`s: it is
    /// what stops two processes taking one each and deadlocking.
    #[test]
    fn a_negative_op_needs_the_whole_amount_at_once() {
        assert_eq!(applied(&[5], &[op(0, -3)]), alloc::vec![(0, 2)]);
        assert_eq!(applied(&[3], &[op(0, -3)]), alloc::vec![(0, 0)]);
        assert_eq!(
            blocks(&[2], &[op(0, -3)]),
            (0, 0),
            "two units do not satisfy a request for three"
        );
    }

    /// `sem_op == 0` is **wait for zero**, not a no-op. A process uses it to
    /// wait until a resource nobody holds; answering `ENOSYS` told it the
    /// kernel had no semaphores at all.
    #[test]
    fn a_zero_op_waits_for_the_value_to_be_zero() {
        assert_eq!(applied(&[0], &[op(0, 0)]), alloc::vec![]);
        assert_eq!(
            blocks(&[1], &[op(0, 0)]),
            (0, 0),
            "a value of 1 is not zero, so wait-for-zero waits"
        );
    }

    /// Operations within one array are applied in order and see each other.
    /// That is what makes `[+1, -1]` on an empty semaphore succeed while
    /// `[-1, +1]` -- the same two operations the other way round -- blocks.
    #[test]
    fn operations_in_one_array_see_each_others_effect() {
        assert_eq!(
            applied(&[0], &[op(0, 1), op(0, -1)]),
            alloc::vec![],
            "up then down nets to no change, and must not block"
        );
        assert_eq!(
            blocks(&[0], &[op(0, -1), op(0, 1)]),
            (0, 0),
            "down then up blocks: the unit is not there yet when it is taken"
        );
    }

    /// A semaphore number outside the set is `EFBIG` -- and it is checked on
    /// every operation, not just the first, because one bad index in the
    /// middle of an array is the one a caller will actually write.
    #[test]
    fn a_semaphore_number_past_the_end_is_efbig() {
        assert_eq!(plan_semop(&[0, 0], &[op(2, 1)]), Err(LxError::EFBIG));
        assert_eq!(
            plan_semop(&[0, 0], &[op(0, 1), op(9, 1)]),
            Err(LxError::EFBIG),
            "a bad index after a good one is still EFBIG, and applies nothing"
        );
    }

    /// An addition that would carry a semaphore past `SEMVMX` is `ERANGE`,
    /// and like every other failure it applies nothing.
    #[test]
    fn an_addition_past_semvmx_is_erange_and_changes_nothing() {
        let max = SEMVMX as isize;
        assert_eq!(applied(&[max - 1], &[op(0, 1)]), alloc::vec![(0, max)]);
        assert_eq!(plan_semop(&[max], &[op(0, 1)]), Err(LxError::ERANGE));
        assert_eq!(
            plan_semop(&[0, 0], &[op(0, 5), op(1, i16::MAX)]),
            Ok(SemopPlan::Apply(alloc::vec![
                (0, 5),
                (1, i16::MAX as isize)
            ])),
            "i16::MAX is under SEMVMX by a wide margin and must be allowed"
        );
    }

    /// `i16::MIN` has no positive counterpart in an `i16`, so negating it
    /// inside that width wraps back to itself -- a request for 32768 units
    /// read as a request for *minus* 32768, which every semaphore satisfies.
    /// The value has to be widened before it is negated.
    #[test]
    fn the_most_negative_op_does_not_wrap_into_a_release() {
        assert_eq!(
            blocks(&[0], &[op(0, i16::MIN)]),
            (0, 0),
            "asking for 32768 units of an empty semaphore must wait"
        );
        assert_eq!(
            plan_semop(&[40_000], &[op(0, i16::MIN)]),
            Ok(SemopPlan::Apply(alloc::vec![(0, 40_000 - 32_768)])),
            "and when they are there, it takes exactly that many"
        );
    }

    /// Only what actually moved is written back. An array of no-ops must not
    /// bump a generation counter or set a readiness flag, or every
    /// wait-for-zero in the system wakes for nothing several times a second.
    #[test]
    fn an_array_that_changes_nothing_writes_nothing() {
        assert_eq!(applied(&[0, 4], &[op(0, 0)]), alloc::vec![]);
        assert_eq!(
            applied(&[4, 9], &[op(0, 2), op(0, -2), op(1, 1)]),
            alloc::vec![(1, 10)],
            "only the semaphore whose value ended up different"
        );
    }

    /// Several semaphores in one array, which is what semop is for: an array
    /// is how a process takes two resources without a lock-ordering
    /// deadlock, so all of them moving together is the point.
    /// The blocking operation is named by its index in the array, not just by
    /// its semaphore. `IPC_NOWAIT` is per-operation, so the caller has to know
    /// WHICH operation could not proceed to decide whether to wait: a
    /// `[+1, (-2 NOWAIT)]` fails with EAGAIN while a `[(+1 NOWAIT), -2]` -- the
    /// same two ops with the flag on the other one -- blocks.
    #[test]
    fn the_blocking_operation_is_named_by_its_place_in_the_array() {
        // The first op succeeds, the second blocks: op_index 1.
        assert_eq!(blocks(&[0], &[op(0, 1), op(0, -2)]), (0, 1));
        // Same array, first op blocks: op_index 0.
        assert_eq!(blocks(&[0], &[op(0, -2), op(0, 1)]), (0, 0));

        // The op that carries IPC_NOWAIT is the second one; it is also the one
        // that blocks, so a caller would answer EAGAIN.
        const NOWAIT: i16 = 0x800;
        let (_, blocking) = blocks(&[0], &[op(0, 1), SemBuf::new(0, -2, NOWAIT)]);
        assert_eq!(blocking, 1, "the NOWAIT op is the one that blocks");

        // Flip the flag onto the op that does NOT block: the blocking op has
        // no NOWAIT, so a caller would wait, not fail.
        let (_, blocking) = blocks(&[0], &[SemBuf::new(0, 1, NOWAIT), op(0, -2)]);
        assert_eq!(
            blocking, 1,
            "the op without NOWAIT is still the one that blocks"
        );
    }

    #[test]
    fn a_multi_semaphore_array_lands_together() {
        assert_eq!(
            applied(&[1, 1, 1], &[op(0, -1), op(1, -1), op(2, -1)]),
            alloc::vec![(0, 0), (1, 0), (2, 0)]
        );
        assert_eq!(
            blocks(&[1, 0, 1], &[op(0, -1), op(1, -1), op(2, -1)]),
            (1, 1),
            "one missing unit holds the whole array"
        );
    }
}

#[cfg(test)]
mod shmat_place_tests {
    //! What `shmat(id, addr, shmflg)` does with its flags and address, which
    //! the old code did not do at all: it answered every attach with one fixed
    //! `READ|WRITE|EXECUTE|USER` and passed `None` as the address, so
    //! `SHM_RDONLY` was ignored (a read-only attach came back writable) and a
    //! requested address was ignored (the segment landed wherever the kernel
    //! chose, and the caller was told so only by the return value).

    use super::{shmat_flags_and_place, ShmatPlace, SHMLBA, SHM_RDONLY, SHM_RND};
    use crate::LxError;
    use zircon_object::vm::*;

    /// A read-only attach must come back read-only. This is the correctness --
    /// and security -- fix: a program that attaches `SHM_RDONLY` and then
    /// stores has to take `SIGSEGV`, not scribble over a segment it asked the
    /// kernel to protect.
    #[test]
    fn shm_rdonly_drops_write() {
        let (flags, _) = shmat_flags_and_place(SHM_RDONLY, 0).unwrap();
        assert!(flags.contains(MMUFlags::READ));
        assert!(
            !flags.contains(MMUFlags::WRITE),
            "SHM_RDONLY must not leave the mapping writable"
        );
        // USER is never dropped, whatever else changes (the glxgears #PF).
        assert!(flags.contains(MMUFlags::USER));
    }

    /// The default attach is read/write. Nothing about adding the read-only
    /// path may quietly make the ordinary attach read-only.
    #[test]
    fn a_default_attach_is_read_write() {
        let (flags, _) = shmat_flags_and_place(0, 0).unwrap();
        assert!(flags.contains(MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER));
    }

    /// `addr == 0` lets the kernel choose -- the MIT-SHM path, and the only
    /// one the old code ever really took. It must stay `Anywhere` regardless
    /// of the other flags.
    #[test]
    fn a_null_address_is_placed_anywhere() {
        assert_eq!(shmat_flags_and_place(0, 0).unwrap().1, ShmatPlace::Anywhere);
        assert_eq!(
            shmat_flags_and_place(SHM_RND, 0).unwrap().1,
            ShmatPlace::Anywhere,
            "SHM_RND on a null address still means 'anywhere', not 'at 0'"
        );
    }

    /// A page-aligned address without `SHM_RND` is honoured as given.
    #[test]
    fn an_aligned_address_is_placed_there() {
        let addr = 8 * SHMLBA;
        assert_eq!(
            shmat_flags_and_place(0, addr).unwrap().1,
            ShmatPlace::At(addr)
        );
    }

    /// A non-aligned address WITHOUT `SHM_RND` is `EINVAL`. The old code
    /// ignored the address entirely, so this request used to succeed at some
    /// unrelated address -- the opposite of what the caller asked for.
    #[test]
    fn a_misaligned_address_without_rnd_is_einval() {
        let addr = 8 * SHMLBA + 1;
        assert_eq!(shmat_flags_and_place(0, addr), Err(LxError::EINVAL));
    }

    /// `SHM_RND` rounds the address DOWN to the nearest `SHMLBA`, and never
    /// up: the segment must not start above where the caller pointed.
    #[test]
    fn shm_rnd_rounds_the_address_down() {
        let below = 8 * SHMLBA;
        // Anywhere inside the page rounds back to its base.
        for extra in [1, 17, SHMLBA - 1] {
            assert_eq!(
                shmat_flags_and_place(SHM_RND, below + extra).unwrap().1,
                ShmatPlace::At(below),
                "SHM_RND must round {} down to {}",
                below + extra,
                below
            );
        }
        // An already-aligned address is left where it is.
        assert_eq!(
            shmat_flags_and_place(SHM_RND, below).unwrap().1,
            ShmatPlace::At(below)
        );
    }

    /// The permission flags and the placement are independent: `SHM_RDONLY`
    /// and `SHM_RND` set together must give a read-only mapping AND a
    /// rounded-down address, not one or the other.
    #[test]
    fn rdonly_and_rnd_both_take_effect_together() {
        let (flags, place) = shmat_flags_and_place(SHM_RDONLY | SHM_RND, 4 * SHMLBA + 3).unwrap();
        assert!(!flags.contains(MMUFlags::WRITE));
        assert_eq!(place, ShmatPlace::At(4 * SHMLBA));
    }
}

#[cfg(test)]
mod ipc_access_tests {
    //! Which access each `semctl` command and each `shmat` needs. The two are
    //! the only places where the answer is not simply "read" or "write": the
    //! rest of the IPC syscalls each want one fixed bit, so they ask for it
    //! inline.

    use super::*;

    /// `IPC_SET` and `IPC_RMID` are not `ipcperms`'s question: they ask who
    /// may *change* the set, which is `may_control`, and they answer `EPERM`.
    #[test]
    fn the_two_control_commands_are_not_an_access_question() {
        assert_eq!(semctl_access(&SemctlCmds::IPC_RMID), None);
        assert_eq!(semctl_access(&SemctlCmds::IPC_SET), None);
    }

    /// Reading a value or the `semid_ds` needs read; writing one needs write.
    #[test]
    fn semctl_splits_its_commands_into_readers_and_writers() {
        for cmd in [
            SemctlCmds::IPC_STAT,
            SemctlCmds::GETPID,
            SemctlCmds::GETVAL,
            SemctlCmds::GETALL,
            SemctlCmds::GETNCNT,
            SemctlCmds::GETZCNT,
        ] {
            assert_eq!(semctl_access(&cmd), Some(IPC_R), "{cmd:?}");
        }
        for cmd in [SemctlCmds::SETVAL, SemctlCmds::SETALL] {
            assert_eq!(semctl_access(&cmd), Some(IPC_W), "{cmd:?}");
        }
    }

    /// `do_shmat`: read always, write unless the caller asked for
    /// `SHM_RDONLY`. Attaching a read-only segment read-write is EACCES, and
    /// that is the whole reason the two are not the same request.
    #[test]
    fn a_read_only_attach_asks_for_less_than_a_writable_one() {
        assert_eq!(shmat_access(0), IPC_R | IPC_W);
        assert_eq!(shmat_access(SHM_RDONLY), IPC_R);
        assert_eq!(shmat_access(SHM_RDONLY | SHM_RND), IPC_R);
        assert_eq!(shmat_access(SHM_RND), IPC_R | IPC_W);
    }
}
