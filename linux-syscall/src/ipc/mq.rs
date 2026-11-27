//! The POSIX message queue syscalls: `mq_open`, `mq_unlink`,
//! `mq_timedsend`, `mq_timedreceive`, `mq_notify`, `mq_getsetattr`
//! (`ipc/mqueue.c`). The queues are [`linux_object::ipc::PosixMq`]; this is
//! the copying in and out, the checks in Linux's order, and the waits.
//!
//! `mq_close` is `close` and `mq_receive`/`mq_send` are the timed calls with
//! no timeout: libc makes them so.

use super::*;
use crate::narrow::Narrow;
use core::convert::TryInto;
use core::time::Duration;
use linux_object::fs::FileLike;
use linux_object::ipc::{
    check_name, deliver, deliver_removed, mq_forget_created, mq_open, mq_unlink, realtime_deadline,
    MqAttr, MqCred, MqFile, MqNotify, MqNotifyHow, MQ_MSG_COST, MQ_PRIO_MAX, NOTIFY_COOKIE_LEN,
};
use linux_object::kmem::Charge;
use linux_object::net::NetlinkSocketState;
use linux_object::process::{CAP_DAC_OVERRIDE, CAP_FOWNER, CAP_SYS_RESOURCE, RLIMIT_MSGQUEUE};
use linux_object::time::TimeSpec;

/// `struct mq_attr` is eight `long`s, so a 32-bit caller's is the native one
/// with every word halved, and signed.
impl Narrow for MqAttr {
    const WORDS: usize = 8;

    fn to_words(self, out: &mut [u64]) {
        out[0] = self.flags as u64;
        out[1] = self.maxmsg as u64;
        out[2] = self.msgsize as u64;
        out[3] = self.curmsgs as u64;
        for (i, r) in self.reserved.iter().enumerate() {
            out[4 + i] = *r as u64;
        }
    }

    fn from_words(words: &[u64]) -> Self {
        MqAttr {
            flags: words[0] as isize,
            maxmsg: words[1] as isize,
            msgsize: words[2] as isize,
            curmsgs: words[3] as isize,
            reserved: [
                words[4] as isize,
                words[5] as isize,
                words[6] as isize,
                words[7] as isize,
            ],
        }
    }
}

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
/// `_NSIG`: `valid_signal` takes 0 to 64.
const NSIG: i32 = 64;
const O_NONBLOCK: isize = 0o4000;

impl Syscall<'_> {
    /// What the queues ask of the caller: its filesystem ids, its groups,
    /// three capabilities, its umask and its `RLIMIT_MSGQUEUE`.
    fn mq_cred(&self) -> MqCred {
        let proc = self.linux_process();
        let cred = proc.credentials();
        let rlimit = proc
            .rlimit(RLIMIT_MSGQUEUE, None, false)
            .map(|l| l.cur as usize)
            .unwrap_or(0);
        MqCred {
            euid: cred.fsuid,
            egid: cred.fsgid,
            ruid: cred.ruid,
            dac_override: cred.capable(CAP_DAC_OVERRIDE),
            fowner: cred.capable(CAP_FOWNER),
            sys_resource: cred.capable(CAP_SYS_RESOURCE),
            umask: cred.umask as u32,
            rlimit_msgqueue: rlimit,
            groups: cred.groups,
        }
    }

    /// The queue descriptor `fd`, or `EBADF` for anything else.
    fn mq_file(&self, fd: usize) -> LxResult<Arc<dyn FileLike>> {
        let file = self
            .linux_process()
            .get_file_like(fd.into())
            .map_err(|_| LxError::EBADF)?;
        if file.downcast_ref::<MqFile>().is_none() {
            return Err(LxError::EBADF);
        }
        Ok(file)
    }

    /// `prepare_timeout`: an absolute `CLOCK_REALTIME` time, checked before
    /// anything else is (`EFAULT`, `EINVAL`), as the `timespec` on the
    /// `timespec` of the caller's width; `None` waits for ever. The answer
    /// is a `timer_now` instant.
    fn mq_deadline(&self, abs_timeout: usize, wide: bool) -> LxResult<Option<Duration>> {
        if abs_timeout == 0 {
            return Ok(None);
        }
        let ptr: UserInPtr<TimeSpec> = abs_timeout.into();
        let ts = crate::narrow::narrow_get(self.narrow_abi() && !wide, &ptr)?;
        let abs = ts.try_into_duration()?;
        Ok(Some(realtime_deadline(
            abs,
            kernel_hal::timer::wall_clock_now(),
            kernel_hal::timer::timer_now(),
        )))
    }

    /// `mq_open(2)`: open the queue `name` names, making it first with
    /// `O_CREAT`. The descriptor is always close-on-exec, as Linux's is.
    pub fn sys_mq_open(&self, name: usize, oflag: usize, mode: usize, attr: usize) -> SysResult {
        // The attributes are copied in whatever happens (`EFAULT`), and
        // looked at only if a queue is made.
        let attr = if attr != 0 {
            let ptr: UserInPtr<MqAttr> = attr.into();
            Some(self.get_narrow(&ptr)?)
        } else {
            None
        };
        let name = UserInPtr::<u8>::from(name).as_c_str()?;
        info!(
            "mq_open: name={:?}, oflag={:#o}, mode={:#o}",
            name, oflag, mode
        );
        check_name(name)?;
        let (queue, created) = mq_open(name, oflag, mode as u32 & 0o7777, attr, &self.mq_cred())?;
        let file = MqFile::new(queue.clone(), oflag);
        match self.linux_process().add_file_cloexec(file, true) {
            Ok(fd) => Ok(fd.into()),
            Err(err) => {
                if created {
                    mq_forget_created(&queue);
                }
                Err(err)
            }
        }
    }

    /// `mq_unlink(2)`.
    pub fn sys_mq_unlink(&self, name: usize) -> SysResult {
        let name = UserInPtr::<u8>::from(name).as_c_str()?;
        info!("mq_unlink: name={:?}", name);
        mq_unlink(name, &self.mq_cred())?;
        Ok(0)
    }

    /// `mq_timedsend(2)`. `wide` is the `_time64` call of a 32-bit process,
    /// whose `timespec` is the native one.
    pub async fn sys_mq_timedsend(
        &self,
        fd: usize,
        msg: usize,
        len: usize,
        prio: usize,
        abs_timeout: usize,
        wide: bool,
    ) -> SysResult {
        info!(
            "mq_timedsend: fd={}, msg={:#x}, len={}, prio={}, timeout={:#x}",
            fd, msg, len, prio, abs_timeout
        );
        let deadline = self.mq_deadline(abs_timeout, wide)?;
        // `unsigned int msg_prio`: the upper half of the register is not
        // part of it.
        let prio = prio as u32;
        if prio as usize >= MQ_PRIO_MAX {
            return Err(LxError::EINVAL);
        }
        let file = self.mq_file(fd)?;
        let mq = file.downcast_ref::<MqFile>().unwrap();
        if !mq.writable() {
            return Err(LxError::EBADF);
        }
        let queue = mq.queue();
        if len > queue.msgsize() {
            return Err(LxError::EMSGSIZE);
        }
        let mut data = UserInPtr::<u8>::from(msg).read_array(len)?;
        let mut charge = Charge::try_new(len + MQ_MSG_COST)?;
        loop {
            match queue.try_send(data, prio, charge) {
                Ok(fired) => {
                    if let Some(fired) = fired {
                        let pid = self.zircon_process().id() as u32;
                        let uid = self.linux_process().credentials().ruid;
                        deliver(fired, pid, uid);
                    }
                    return Ok(0);
                }
                Err((back, back_charge)) => {
                    if mq.nonblock() {
                        return Err(LxError::EAGAIN);
                    }
                    data = back;
                    charge = back_charge;
                }
            }
            queue.wait_room(deadline).await?;
        }
    }

    /// `mq_timedreceive(2)`: the length of the message taken, its priority
    /// at `prio` when that is not null.
    pub async fn sys_mq_timedreceive(
        &self,
        fd: usize,
        msg: usize,
        len: usize,
        prio: usize,
        abs_timeout: usize,
        wide: bool,
    ) -> SysResult {
        info!(
            "mq_timedreceive: fd={}, msg={:#x}, len={}, prio={:#x}, timeout={:#x}",
            fd, msg, len, prio, abs_timeout
        );
        let deadline = self.mq_deadline(abs_timeout, wide)?;
        let file = self.mq_file(fd)?;
        let mq = file.downcast_ref::<MqFile>().unwrap();
        if !mq.readable() {
            return Err(LxError::EBADF);
        }
        let queue = mq.queue();
        if len < queue.msgsize() {
            return Err(LxError::EMSGSIZE);
        }
        loop {
            if let Some((data, got)) = queue.try_recv() {
                // Taken off the queue either way, as in Linux: a fault on the
                // way out loses the message.
                UserOutPtr::<u8>::from(msg).write_array(&data)?;
                if prio != 0 {
                    UserOutPtr::<u32>::from(prio).write_unaligned(got)?;
                }
                return Ok(data.len());
            }
            if mq.nonblock() {
                return Err(LxError::EAGAIN);
            }
            queue.wait_message(deadline).await?;
        }
    }

    /// `mq_notify(2)`: register the caller for a message into the empty
    /// queue, or take its registration away with a null `sevp`.
    pub fn sys_mq_notify(&self, fd: usize, sevp: usize) -> SysResult {
        info!("mq_notify: fd={}, sevp={:#x}", fd, sevp);
        let how = if sevp != 0 {
            let ev = crate::narrow::sigevent::sigevent_get(self.narrow_abi(), sevp)?;
            Some(match ev.notify {
                SIGEV_NONE => MqNotifyHow::None,
                SIGEV_SIGNAL => {
                    if !(0..=NSIG).contains(&ev.signo) {
                        return Err(LxError::EINVAL);
                    }
                    MqNotifyHow::Signal {
                        signo: ev.signo as usize,
                        value: ev.value,
                    }
                }
                SIGEV_THREAD => self.mq_notify_socket(ev.signo, ev.value)?,
                _ => return Err(LxError::EINVAL),
            })
        } else {
            None
        };
        let file = self.mq_file(fd)?;
        let queue = file.downcast_ref::<MqFile>().unwrap().queue();
        let owner = self.zircon_process().id();
        match how {
            None => {
                if let Some(removed) = queue.notify_unregister(owner) {
                    deliver_removed(removed);
                }
            }
            Some(how) => queue.notify_register(MqNotify { owner, how })?,
        }
        Ok(0)
    }

    /// `SIGEV_THREAD` as libc hands it over: `sigev_signo` is a netlink
    /// socket and `sigev_value` points at the cookie to send on it.
    fn mq_notify_socket(&self, fd: i32, cookie: usize) -> LxResult<MqNotifyHow> {
        let cookie: [u8; NOTIFY_COOKIE_LEN] = UserInPtr::<u8>::from(cookie)
            .read_array(NOTIFY_COOKIE_LEN)?
            .try_into()
            .map_err(|_| LxError::EFAULT)?;
        let sock = self
            .linux_process()
            .get_file_like((fd as usize).into())
            .map_err(|_| LxError::EBADF)?;
        // `netlink_getsockbyfilp`: not a socket, and a socket of another
        // family, are two different answers.
        if sock.downcast_ref::<NetlinkSocketState>().is_none() {
            return Err(if sock.is_socket() {
                LxError::ECONNREFUSED
            } else {
                LxError::ENOTSOCK
            });
        }
        Ok(MqNotifyHow::Thread { sock, cookie })
    }

    /// `mq_getsetattr(2)`: the attributes at `old`, and `O_NONBLOCK` from
    /// `new`, the only thing in it that can change.
    pub fn sys_mq_getsetattr(&self, fd: usize, new: usize, old: usize) -> SysResult {
        info!("mq_getsetattr: fd={}, new={:#x}, old={:#x}", fd, new, old);
        let new = if new != 0 {
            let ptr: UserInPtr<MqAttr> = new.into();
            let attr = self.get_narrow(&ptr)?;
            if attr.flags & !O_NONBLOCK != 0 {
                return Err(LxError::EINVAL);
            }
            Some(attr)
        } else {
            None
        };
        let file = self.mq_file(fd)?;
        let was = file
            .downcast_ref::<MqFile>()
            .unwrap()
            .getsetattr(new.as_ref());
        if old != 0 {
            let mut ptr: UserOutPtr<MqAttr> = old.into();
            self.put_narrow(&mut ptr, was)?;
        }
        Ok(0)
    }

    /// `mqueue_flush_file`: a `close` by the process registered on the
    /// queue takes its registration away, whichever descriptor it closes.
    pub(crate) fn mq_flush(&self, file: &Arc<dyn FileLike>) {
        if let Some(mq) = file.downcast_ref::<MqFile>() {
            if let Some(removed) = mq.queue().notify_unregister(self.zircon_process().id()) {
                deliver_removed(removed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mq_attr_is_eight_words_at_either_width() {
        assert_eq!(core::mem::size_of::<MqAttr>(), MqAttr::WORDS * 8);
        let a = MqAttr {
            flags: O_NONBLOCK,
            maxmsg: 10,
            msgsize: -1,
            curmsgs: 3,
            reserved: [0; 4],
        };
        let mut words = [0u64; 8];
        a.to_words(&mut words);
        assert_eq!(MqAttr::from_words(&words), a);
    }
}
