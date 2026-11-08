//! Linux syscalls answered by a userspace server.
//!
//! This is the first piece of the Linux personality outside the kernel. A
//! process hands the kernel one end of a channel and a set of Linux syscall
//! numbers; from then on each of those syscalls, instead of running in the
//! kernel, becomes a channel call to whoever holds the other end. The server
//! reads the request, does the work and replies with the return value, or
//! replies "native" to have the kernel run the syscall itself, which is how
//! a server takes over a personality one syscall at a time.
//!
//! The request is built by the kernel, so the server can trust who it comes
//! from: the pid and tid in it are the caller's (a *badge*, in the
//! microkernel sense), not something the client wrote.
//!
//! A redirect is for good. It is set once, a second one is refused, it is
//! inherited by `fork` and kept across `execve`: a server confines a program
//! by forking, redirecting the child, and having the child exec it.
//!
//! The wire format, little-endian:
//!
//! ```text
//! request (REQUEST_LEN = 80 bytes)       reply (at least REPLY_LEN = 16 bytes)
//!   0  u32 txid (the kernel's)             0  u32 txid (the request's)
//!   4  u32 kind = KIND_SYSCALL             4  u32 flags (REPLY_NATIVE)
//!   8  u64 pid                             8  i64 return value
//!  16  u64 tid
//!  24  u64 syscall number
//!  32  u64 args[6]
//! ```

use super::*;
use core::convert::TryInto;
use zircon_object::ipc::{Channel, MessagePacket};

/// Syscall numbers a redirect can name: the mask is this many bits.
pub const MAX_REDIRECTED: usize = 512;

/// The length of a request.
pub const REQUEST_LEN: usize = 80;
/// The shortest reply the kernel takes.
pub const REPLY_LEN: usize = 16;
/// `kind` of a syscall request, the only one so far.
pub const KIND_SYSCALL: u32 = 1;
/// Reply flag: run the syscall in the kernel after all; the reply's return
/// value is ignored.
pub const REPLY_NATIVE: u32 = 1;

/// What became of a redirected syscall.
#[derive(Debug, PartialEq, Eq)]
pub enum Forwarded {
    /// The server answered: this is the return value, as the syscall gives
    /// it (a negative errno on failure).
    Return(isize),
    /// The server asked for the syscall to run in the kernel.
    Native,
}

/// One process's redirect: where the syscalls go and which ones.
pub struct SyscallRedirect {
    channel: Arc<Channel>,
    mask: [u64; MAX_REDIRECTED / 64],
}

impl SyscallRedirect {
    /// Redirect the syscalls whose bits are set in `mask` to the peer of
    /// `channel`.
    pub fn new(channel: Arc<Channel>, mask: [u64; MAX_REDIRECTED / 64]) -> Arc<Self> {
        Arc::new(SyscallRedirect { channel, mask })
    }

    /// Whether syscall `num` goes to the server.
    pub fn covers(&self, num: usize) -> bool {
        num < MAX_REDIRECTED && self.mask[num / 64] & (1 << (num % 64)) != 0
    }

    /// Send syscall `num` with `args` from thread `tid` of process `pid` to
    /// the server and wait for its answer. A server that is gone answers
    /// `ENOSYS` for every syscall it took, and a reply that is not one
    /// answers `EIO`; neither runs the syscall in the kernel, since
    /// a redirected process is confined by its server and must stay so when
    /// the server dies.
    ///
    /// Cancelling the future (a signal) cancels the call, and its reply, if
    /// it comes, is dropped by the next request.
    pub async fn forward(&self, pid: u64, tid: u64, num: usize, args: &[usize; 6]) -> Forwarded {
        self.drop_late_replies();
        let mut data = Vec::with_capacity(REQUEST_LEN);
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&KIND_SYSCALL.to_le_bytes());
        data.extend_from_slice(&pid.to_le_bytes());
        data.extend_from_slice(&tid.to_le_bytes());
        data.extend_from_slice(&(num as u64).to_le_bytes());
        for a in args {
            data.extend_from_slice(&(*a as u64).to_le_bytes());
        }
        let request = MessagePacket {
            data,
            handles: Vec::new(),
        };
        match self.channel.call(request).await {
            Ok(reply) => Self::answer(&reply),
            Err(_) => Forwarded::Return(-(LxError::ENOSYS as isize)),
        }
    }

    fn answer(reply: &MessagePacket) -> Forwarded {
        if reply.data.len() < REPLY_LEN || !reply.handles.is_empty() {
            return Forwarded::Return(-(LxError::EIO as isize));
        }
        let word = |at: usize| u32::from_le_bytes(reply.data[at..at + 4].try_into().unwrap());
        if word(4) & REPLY_NATIVE != 0 {
            return Forwarded::Native;
        }
        let ret = i64::from_le_bytes(reply.data[8..16].try_into().unwrap());
        Forwarded::Return(ret as isize)
    }

    /// Nothing but replies ever arrives on the kernel's end, and every call
    /// takes its own out of the reply table, so whatever is queued there is
    /// the reply to a call that was cancelled.
    fn drop_late_replies(&self) {
        if self.channel.signal().contains(Signal::READABLE) {
            while self.channel.read().is_ok() {}
        }
    }
}

/// Where a process keeps its redirect: set at most once, read lock-free on
/// every syscall.
#[derive(Default)]
pub struct RedirectSlot(spin::Once<Arc<SyscallRedirect>>);

impl RedirectSlot {
    /// The slot of a forked child: the parent's redirect, if it has one.
    pub fn forked(parent: &RedirectSlot) -> Self {
        match parent.get() {
            Some(r) => RedirectSlot(spin::Once::initialized(r.clone())),
            None => RedirectSlot::default(),
        }
    }

    /// The redirect, if there is one.
    #[inline]
    pub fn get(&self) -> Option<&Arc<SyscallRedirect>> {
        self.0.get()
    }

    /// Install `redirect`. `ALREADY_BOUND` if the process has one.
    pub fn set(&self, redirect: Arc<SyscallRedirect>) -> ZxResult {
        let mut won = false;
        self.0.call_once(|| {
            won = true;
            redirect
        });
        if won {
            Ok(())
        } else {
            Err(ZxError::ALREADY_BOUND)
        }
    }
}

impl LinuxProcess {
    /// This process's syscall redirect slot.
    pub fn syscall_redirect(&self) -> &RedirectSlot {
        &self.syscall_redirect
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::task::block_on;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    fn mask_of(nums: &[usize]) -> [u64; MAX_REDIRECTED / 64] {
        let mut mask = [0; MAX_REDIRECTED / 64];
        for &n in nums {
            mask[n / 64] |= 1 << (n % 64);
        }
        mask
    }

    fn reply(request: &MessagePacket, flags: u32, ret: i64) -> MessagePacket {
        let mut data = Vec::new();
        data.extend_from_slice(&request.get_txid().to_le_bytes());
        data.extend_from_slice(&flags.to_le_bytes());
        data.extend_from_slice(&ret.to_le_bytes());
        MessagePacket {
            data,
            handles: Vec::new(),
        }
    }

    /// Run `forward` until it sends, let `serve` answer, and finish it.
    fn round_trip(
        redirect: &SyscallRedirect,
        server: &Channel,
        serve: impl FnOnce(&MessagePacket) -> MessagePacket,
    ) -> (MessagePacket, Forwarded) {
        let mut cx = Context::from_waker(Waker::noop());
        let mut call = pin!(redirect.forward(7, 9, 39, &[1, 2, 3, 4, 5, 6]));
        assert!(call.as_mut().poll(&mut cx).is_pending());
        let request = server.read().unwrap();
        server.write(serve(&request)).unwrap();
        match call.as_mut().poll(&mut cx) {
            Poll::Ready(f) => (request, f),
            Poll::Pending => panic!("the reply did not reach the call"),
        }
    }

    #[test]
    fn the_mask_names_the_redirected_syscalls_and_nothing_past_it() {
        let (a, _b) = Channel::create();
        let r = SyscallRedirect::new(a, mask_of(&[0, 39, 63, 64, 511]));
        for n in [0, 39, 63, 64, 511] {
            assert!(r.covers(n), "{}", n);
        }
        for n in [1, 40, 65, 510, 512, 0x5a00_0000] {
            assert!(!r.covers(n), "{}", n);
        }
    }

    #[test]
    fn the_request_carries_the_callers_identity_and_the_reply_its_value() {
        let (a, b) = Channel::create();
        let r = SyscallRedirect::new(a, mask_of(&[39]));
        let (request, answer) = round_trip(&r, &b, |req| reply(req, 0, 4242));
        assert_eq!(answer, Forwarded::Return(4242));
        let d = &request.data;
        assert_eq!(d.len(), REQUEST_LEN);
        let u64_at = |at: usize| u64::from_le_bytes(d[at..at + 8].try_into().unwrap());
        assert_eq!(
            u32::from_le_bytes(d[4..8].try_into().unwrap()),
            KIND_SYSCALL
        );
        assert_eq!((u64_at(8), u64_at(16), u64_at(24)), (7, 9, 39));
        let args: Vec<u64> = (0..6).map(|i| u64_at(32 + 8 * i)).collect();
        assert_eq!(args, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_native_reply_hands_the_syscall_back_and_a_bad_one_is_eio() {
        let (a, b) = Channel::create();
        let r = SyscallRedirect::new(a, mask_of(&[39]));
        let (_, answer) = round_trip(&r, &b, |req| reply(req, REPLY_NATIVE, -1));
        assert_eq!(answer, Forwarded::Native);
        let (_, answer) = round_trip(&r, &b, |req| {
            let mut short = reply(req, 0, 0);
            short.data.truncate(12);
            short
        });
        assert_eq!(answer, Forwarded::Return(-(LxError::EIO as isize)));
    }

    #[test]
    fn a_server_that_is_gone_answers_enosys() {
        let (a, b) = Channel::create();
        let r = SyscallRedirect::new(a, mask_of(&[39]));
        drop(b);
        let answer = block_on(r.forward(1, 1, 39, &[0; 6]));
        assert_eq!(answer, Forwarded::Return(-(LxError::ENOSYS as isize)));
    }

    #[test]
    fn a_late_reply_to_a_cancelled_call_does_not_stay_queued() {
        let (a, b) = Channel::create();
        let r = SyscallRedirect::new(a.clone(), mask_of(&[39]));
        let mut cx = Context::from_waker(Waker::noop());
        let late = {
            let mut call = pin!(r.forward(1, 1, 39, &[0; 6]));
            assert!(call.as_mut().poll(&mut cx).is_pending());
            b.read().unwrap()
        };
        b.write(reply(&late, 0, 1)).unwrap();
        assert!(a.signal().contains(Signal::READABLE));
        let (_, answer) = round_trip(&r, &b, |req| reply(req, 0, 2));
        assert_eq!(answer, Forwarded::Return(2));
        assert!(!a.signal().contains(Signal::READABLE));
    }

    #[test]
    fn a_slot_is_set_once_and_a_fork_inherits_it() {
        let (a, _b) = Channel::create();
        let slot = RedirectSlot::default();
        assert!(slot.get().is_none());
        assert!(RedirectSlot::forked(&slot).get().is_none());
        slot.set(SyscallRedirect::new(a.clone(), mask_of(&[1])))
            .unwrap();
        assert_eq!(
            slot.set(SyscallRedirect::new(a, mask_of(&[2]))).err(),
            Some(ZxError::ALREADY_BOUND)
        );
        let child = RedirectSlot::forked(&slot);
        assert!(child.get().unwrap().covers(1));
        assert!(Arc::ptr_eq(child.get().unwrap(), slot.get().unwrap()));
    }
}
