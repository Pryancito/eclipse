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
//!  16  u64 tid                           16  u64 received (optional)
//!  24  u64 syscall number                 24  u64 replied (optional)
//!  32  u64 args[6]
//! ```
//!
//! `received` and `replied` are when the server read the request and when
//! it sent the reply, in `CLOCK_MONOTONIC` nanoseconds; a server that fills
//! them in (a reply of TIMED_REPLY_LEN bytes, both non-zero) lets the kernel
//! split each round trip into its phases ([`phases`]), shown in
//! `/proc/perf/kernel`.
//!
//! A syscall's pointer arguments point into the client: the server reaches
//! that memory with `eclipse_redirect_memory`, which [`client_of`] gates.

use super::*;
use core::convert::TryInto;
use zircon_object::ipc::{Channel, MessagePacket};

/// Syscall numbers a redirect can name: the mask is this many bits.
pub const MAX_REDIRECTED: usize = 512;

/// The length of a request.
pub const REQUEST_LEN: usize = 80;
/// The shortest reply the kernel takes.
pub const REPLY_LEN: usize = 16;
/// The length of a reply that says when the server received the request
/// and when it replied.
pub const TIMED_REPLY_LEN: usize = 32;
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

    /// Whether `server_end` is the server's end of this redirect: the peer
    /// of the kernel's. Holding it is the authority to answer for the
    /// process, and so to reach its memory ([`client_of`]).
    pub fn served_by(&self, server_end: &dyn KernelObject) -> bool {
        match server_end.peer() {
            Ok(peer) => peer.id() == self.channel.id(),
            Err(_) => false,
        }
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
        let start = phases::now();
        let reply = match self.channel.call_counted(request) {
            Ok((_, reply)) => reply,
            Err(_) => return Forwarded::Return(-(LxError::ENOSYS as isize)),
        };
        let sent = phases::now();
        match reply.await {
            Ok(reply) => {
                phases::record(start, sent, phases::server_stamps(&reply), phases::now());
                Self::answer(&reply)
            }
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

/// The process `pid` (or the process of thread `pid`) if its syscalls are
/// redirected to `server_end`, for the server to read and write its memory:
/// a syscall's pointer arguments point there. `NOT_FOUND` if there is no
/// such live process, `ACCESS_DENIED` if it is not this server's.
///
/// The channel is the whole authority, as a handle is in Zircon: the client
/// gave its syscalls to whoever holds that end, and answering `read` means
/// writing into its buffer. No ptrace-style credential check applies, the
/// same way none applies to the kernel doing it.
pub fn client_of(server_end: &Arc<Channel>, pid: KoID) -> ZxResult<Arc<Process>> {
    let target = find_task(pid)
        .or_else(|| process_of_thread(pid))
        .ok_or(ZxError::NOT_FOUND)?;
    if matches!(target.status(), Status::Exited(_)) {
        return Err(ZxError::NOT_FOUND);
    }
    let linux = target.try_linux().ok_or(ZxError::NOT_FOUND)?;
    match linux.syscall_redirect.get() {
        Some(r) if r.served_by(server_end.as_ref()) => Ok(target),
        _ => Err(ZxError::ACCESS_DENIED),
    }
}

/// Where the time of a redirected syscall goes, summed over every one the
/// kernel forwarded since boot.
///
/// A round trip, as the kernel sees it from the client's side:
///
/// ```text
/// start -- send --> sent -- to server --> received -- in server --> replied -- back --> done
///          (kernel)        (wake, switch,              (the server's           (its write,
///                           server's read)               own work)               wake, switch)
/// ```
///
/// `send` and the total are the kernel's own clock and always counted. The
/// middle three need the server's `received` and `replied` stamps, and are
/// counted only when the stamps fall in order between `sent` and `done`: a
/// clock that is not in step across CPUs would put them out of order, and
/// that is counted too, as `skewed`.
pub mod phases {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

    /// The phases, in the order of [`Snapshot::sum_ns`].
    pub const NAMES: [&str; 5] = ["send", "to server", "in server", "back", "total"];

    static CALLS: AtomicU64 = AtomicU64::new(0);
    static TIMED: AtomicU64 = AtomicU64::new(0);
    static SKEWED: AtomicU64 = AtomicU64::new(0);
    static SUM_NS: [AtomicU64; 5] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];

    /// What [`snapshot`] answers.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Snapshot {
        /// Round trips that got a reply.
        pub calls: u64,
        /// Of those, the ones whose server stamps were in order.
        pub timed: u64,
        /// The ones whose stamps were there but out of order.
        pub skewed: u64,
        /// Nanoseconds per phase ([`NAMES`]): `send` and `total` over
        /// `calls`, the other three over `timed`.
        pub sum_ns: [u64; 5],
    }

    pub(super) fn now() -> u64 {
        kernel_hal::timer::timer_now().as_nanos() as u64
    }

    /// The server's `(received, replied)` stamps, if the reply has them.
    pub fn server_stamps(reply: &MessagePacket) -> Option<(u64, u64)> {
        if reply.data.len() < TIMED_REPLY_LEN {
            return None;
        }
        let at = |i: usize| u64::from_le_bytes(reply.data[i..i + 8].try_into().unwrap());
        let (received, replied) = (at(16), at(24));
        if received != 0 && replied != 0 {
            Some((received, replied))
        } else {
            None
        }
    }

    /// Count one round trip.
    pub fn record(start: u64, sent: u64, stamps: Option<(u64, u64)>, done: u64) {
        CALLS.fetch_add(1, Relaxed);
        SUM_NS[0].fetch_add(sent.saturating_sub(start), Relaxed);
        SUM_NS[4].fetch_add(done.saturating_sub(start), Relaxed);
        match stamps {
            Some((received, replied))
                if sent <= received && received <= replied && replied <= done =>
            {
                TIMED.fetch_add(1, Relaxed);
                SUM_NS[1].fetch_add(received - sent, Relaxed);
                SUM_NS[2].fetch_add(replied - received, Relaxed);
                SUM_NS[3].fetch_add(done - replied, Relaxed);
            }
            Some(_) => {
                SKEWED.fetch_add(1, Relaxed);
            }
            None => {}
        }
    }

    /// The counts since boot.
    pub fn snapshot() -> Snapshot {
        Snapshot {
            calls: CALLS.load(Relaxed),
            timed: TIMED.load(Relaxed),
            skewed: SKEWED.load(Relaxed),
            sum_ns: core::array::from_fn(|i| SUM_NS[i].load(Relaxed)),
        }
    }

    /// The `/proc/perf/kernel` lines, or nothing before the first call.
    pub fn report(s: &Snapshot) -> String {
        if s.calls == 0 {
            return String::new();
        }
        let mean = |sum: u64, n: u64| {
            if n == 0 {
                0.0
            } else {
                sum as f64 / n as f64 / 1000.0
            }
        };
        let mut out = alloc::format!(
            "syscall redirect: {} round trips, mean {:.1} us (send {:.1} us); \
             {} timed by the server, {} skewed\n",
            s.calls,
            mean(s.sum_ns[4], s.calls),
            mean(s.sum_ns[0], s.calls),
            s.timed,
            s.skewed
        );
        if s.timed != 0 {
            out += &alloc::format!(
                "  redirect phases (mean us): to server {:.1}, in server {:.1}, back {:.1}\n",
                mean(s.sum_ns[1], s.timed),
                mean(s.sum_ns[2], s.timed),
                mean(s.sum_ns[3], s.timed)
            );
        }
        out
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
    fn the_phases_count_what_is_in_order_and_only_that() {
        // `record` adds to counters other tests add to as well: compare
        // differences, of what only this test puts in.
        let before = phases::snapshot();
        phases::record(100, 110, Some((130, 170)), 200);
        phases::record(100, 110, Some((90, 170)), 200); // received before sent
        phases::record(100, 120, None, 150);
        let after = phases::snapshot();
        assert!(after.calls - before.calls >= 3);
        assert!(after.timed - before.timed >= 1);
        assert!(after.skewed - before.skewed >= 1);
    }

    #[test]
    fn the_report_splits_the_phases_once_a_server_stamps_its_replies() {
        let mut s = phases::Snapshot {
            calls: 4,
            timed: 0,
            skewed: 0,
            sum_ns: [4_000, 0, 0, 0, 40_000],
        };
        let r = phases::report(&s);
        assert!(
            r.contains("4 round trips, mean 10.0 us (send 1.0 us)"),
            "{}",
            r
        );
        assert!(!r.contains("phases"), "{}", r);
        s.timed = 2;
        s.sum_ns[1..4].copy_from_slice(&[6_000, 2_000, 8_000]);
        let r = phases::report(&s);
        assert!(
            r.contains("to server 3.0, in server 1.0, back 4.0"),
            "{}",
            r
        );
        assert_eq!(phases::report(&phases::Snapshot { calls: 0, ..s }), "");
    }

    #[test]
    fn a_timed_reply_carries_the_servers_stamps_and_a_plain_one_none() {
        let (a, b) = Channel::create();
        let r = SyscallRedirect::new(a, mask_of(&[39]));
        let (_, answer) = round_trip(&r, &b, |req| {
            let mut timed = reply(req, 0, 5);
            timed.data.extend_from_slice(&11u64.to_le_bytes());
            timed.data.extend_from_slice(&12u64.to_le_bytes());
            assert_eq!(phases::server_stamps(&timed), Some((11, 12)));
            assert_eq!(phases::server_stamps(&reply(req, 0, 5)), None);
            timed
        });
        assert_eq!(answer, Forwarded::Return(5));
    }

    #[test]
    fn only_the_peer_of_the_kernels_end_serves_the_redirect() {
        let (a, b) = Channel::create();
        let (c, d) = Channel::create();
        let r = SyscallRedirect::new(a.clone(), mask_of(&[39]));
        assert!(r.served_by(b.as_ref()));
        assert!(!r.served_by(a.as_ref()));
        assert!(!r.served_by(c.as_ref()));
        assert!(!r.served_by(d.as_ref()));
        drop(a);
        // The kernel's end lives in the redirect, whatever became of the
        // Arc it was made from.
        assert!(r.served_by(b.as_ref()));
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
