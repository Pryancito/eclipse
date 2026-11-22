//! What `socket(2)` and `socketpair(2)` answer for a type or a family they
//! do not serve, as `__sock_create`, `unix_create`, `inet_create` and
//! `__sys_socketpair` answer it.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const AF_UNIX: usize = 1;
const AF_INET: usize = 2;
const AF_INET6: usize = 10;
/// A family number Linux has (`AF_APPLETALK`) that this kernel does not.
const AF_APPLETALK: usize = 5;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOCK_RAW: usize = 3;
const SOCK_RDM: usize = 4;
const SOCK_SEQPACKET: usize = 5;
/// Past `SOCK_MAX`.
const NOT_A_TYPE: usize = 99;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "sockets",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap()
}

fn own<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

#[test]
fn a_unix_raw_socket_is_a_datagram_socket_and_an_unknown_type_esocktnosupport() {
    let proc = a_process(43_110);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            assert!(
                s.sys_socket(AF_UNIX, SOCK_RAW, 0).is_ok(),
                "unix_create takes SOCK_RAW"
            );
            assert_eq!(
                s.sys_socket(AF_UNIX, SOCK_RDM, 0),
                Err(LxError::ESOCKTNOSUPPORT)
            );
            assert_eq!(s.sys_socket(AF_UNIX, NOT_A_TYPE, 0), Err(LxError::EINVAL));
            assert_eq!(
                s.sys_socket(AF_INET, SOCK_SEQPACKET, 0),
                Err(LxError::ESOCKTNOSUPPORT)
            );
            assert_eq!(
                s.sys_socket(AF_INET6, SOCK_RDM, 0),
                Err(LxError::ESOCKTNOSUPPORT)
            );
        })
    });
}

#[test]
fn socketpair_of_a_family_without_one_is_eopnotsupp() {
    let proc = a_process(43_111);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            let mut sv = [-1i32; 2];
            let addr = sv.as_mut_ptr() as usize;
            let at = || UserOutPtr::<i32>::from(addr);
            assert_eq!(
                s.sys_socketpair(AF_INET, SOCK_STREAM, 0, at()),
                Err(LxError::EOPNOTSUPP)
            );
            assert_eq!(
                s.sys_socketpair(AF_INET6, SOCK_DGRAM, 0, at()),
                Err(LxError::EOPNOTSUPP)
            );
            assert_eq!(
                s.sys_socketpair(AF_APPLETALK, SOCK_DGRAM, 0, at()),
                Err(LxError::EAFNOSUPPORT)
            );
            assert_eq!(
                s.sys_socketpair(AF_INET, NOT_A_TYPE, 0, at()),
                Err(LxError::EINVAL)
            );
            assert_eq!(s.sys_socketpair(AF_UNIX, SOCK_RAW, 0, at()), Ok(0));
        })
    });
}

/// A `sockaddr_un` for `path`, and the length to pass with it.
fn sockaddr_un(path: &str) -> ([u8; 110], usize) {
    let mut sa = [0u8; 110];
    sa[..2].copy_from_slice(&(AF_UNIX as u16).to_ne_bytes());
    sa[2..2 + path.len()].copy_from_slice(path.as_bytes());
    (sa, 2 + path.len() + 1)
}

/// `connect(2)` of a unix datagram socket to a bound, non-listening one
/// names its destination (`unix_dgram_connect`); a stream connecting to
/// that address is `EPROTOTYPE` (`unix_find_other` matches the type).
#[test]
fn a_datagram_connect_reaches_a_bound_socket_and_a_stream_is_eprototype() {
    let proc = a_process(43_112);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            let (sa, len) = sockaddr_un("/run/socket-create-dgram");
            let addr = sa.as_ptr() as usize;
            let rx = s.sys_socket(AF_UNIX, SOCK_DGRAM, 0).unwrap();
            assert_eq!(s.sys_bind(rx, addr.into(), len).await, Ok(0));
            let tx = s.sys_socket(AF_UNIX, SOCK_DGRAM, 0).unwrap();
            assert_eq!(s.sys_connect(tx, addr.into(), len).await, Ok(0));
            let stream = s.sys_socket(AF_UNIX, SOCK_STREAM, 0).unwrap();
            assert_eq!(
                s.sys_connect(stream, addr.into(), len).await,
                Err(LxError::EPROTOTYPE)
            );
        })
    });
}

/// `getsockopt(2)` in `__sys_getsockopt`'s order: the fd first, then
/// `optlen`, and a NULL `optval` is only a fault when something would be
/// copied into it.
#[test]
fn getsockopt_checks_the_fd_then_optlen_and_null_optval_is_a_copy_fault() {
    let proc = a_process(43_113);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            const SOL_SOCKET: usize = 1;
            const SO_TYPE: usize = 3;
            const SOL_PACKET: usize = 263;
            let fd = s.sys_socket(AF_UNIX, SOCK_STREAM, 0).unwrap();
            let mut len = [4u32];
            let len_at = len.as_mut_ptr() as usize;
            let mut val = [0u32];
            let val_at = val.as_mut_ptr() as usize;
            // An unknown level on a closed fd is about the fd.
            assert_eq!(
                s.sys_getsockopt(999, SOL_PACKET, 1, val_at.into(), len_at.into()),
                Err(LxError::EBADF)
            );
            assert_eq!(
                s.sys_getsockopt(fd, SOL_PACKET, 1, val_at.into(), len_at.into()),
                Err(LxError::ENOPROTOOPT)
            );
            // NULL with room to copy into is EFAULT; with none it is fine.
            assert_eq!(
                s.sys_getsockopt(fd, SOL_SOCKET, SO_TYPE, 0.into(), len_at.into()),
                Err(LxError::EFAULT)
            );
            let mut zero = [0u32];
            let zero_at = zero.as_mut_ptr() as usize;
            assert_eq!(
                s.sys_getsockopt(fd, SOL_SOCKET, SO_TYPE, 0.into(), zero_at.into()),
                Ok(0)
            );
            // A negative optlen is EINVAL.
            let mut neg = [u32::MAX];
            let neg_at = neg.as_mut_ptr() as usize;
            assert_eq!(
                s.sys_getsockopt(fd, SOL_SOCKET, SO_TYPE, val_at.into(), neg_at.into()),
                Err(LxError::EINVAL)
            );
        })
    });
}

/// `recvmmsg(2)` stops waiting after the first datagram only under
/// `MSG_WAITFORONE`, and a timeout is checked like `do_recvmmsg` checks it:
/// a bad one is `EINVAL` before the fd, and what was left is written back.
#[test]
fn recvmmsg_waits_for_one_only_when_asked_and_takes_a_timeout() {
    let proc = a_process(43_114);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            const MSG_WAITFORONE: usize = 0x1_0000;
            let mut sv = [-1i32; 2];
            let sv_at = sv.as_mut_ptr() as usize;
            assert_eq!(
                s.sys_socketpair(AF_UNIX, SOCK_DGRAM, 0, sv_at.into()),
                Ok(0)
            );
            let (a, b) = (sv[0] as usize, sv[1] as usize);
            let msg = *b"uno";
            let msg_at = msg.as_ptr() as usize;
            assert_eq!(s.sys_write(b.into(), msg_at.into(), 3).await, Ok(3));
            // Two `mmsghdr`s of 64 bytes, each with one 16-byte iovec.
            let mut bufs = [[0u8; 16]; 2];
            let mut iovs = [[0u64; 2]; 2];
            let mut hdrs = [[0u64; 8]; 2];
            for i in 0..2 {
                iovs[i] = [bufs[i].as_mut_ptr() as u64, 16];
                hdrs[i][2] = iovs[i].as_ptr() as u64;
                hdrs[i][3] = 1;
            }
            let hdrs_at = hdrs.as_mut_ptr() as usize;
            // A timeout with a bad tv_nsec, on a closed fd: the timeout first.
            let mut bad = [0u64, 1_000_000_000];
            let bad_at = bad.as_mut_ptr() as usize;
            assert_eq!(
                s.sys_recvmmsg(999, hdrs_at.into(), 2, 0, bad_at.into())
                    .await,
                Err(LxError::EINVAL)
            );
            let mut ts = [5u64, 0];
            let ts_at = ts.as_mut_ptr() as usize;
            assert_eq!(
                s.sys_recvmmsg(a, hdrs_at.into(), 2, MSG_WAITFORONE, ts_at.into())
                    .await,
                Ok(1)
            );
            assert_eq!(&bufs[0][..3], b"uno");
            assert_eq!(hdrs[0][7] as u32, 3, "msg_len");
            assert!(ts[0] <= 5, "what was left is written back");
        })
    });
}

/// The fd is looked at before the address (`__sys_connect`,
/// `__sys_bind`, `__sys_sendmmsg`), an address longer than a
/// `sockaddr_storage` is `EINVAL`, and `AF_UNSPEC` dissolves a datagram
/// socket's destination while a stream refuses it.
#[test]
fn the_fd_comes_before_the_address_and_af_unspec_disconnects_a_datagram() {
    let proc = a_process(43_115);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            let (sa, len) = sockaddr_un("/run/socket-create-unspec");
            let addr = sa.as_ptr() as usize;
            // A bad fd and an address that is not even readable: EBADF.
            assert_eq!(
                s.sys_connect(999, 0x11.into(), len).await,
                Err(LxError::EBADF)
            );
            assert_eq!(s.sys_bind(999, 0x11.into(), len).await, Err(LxError::EBADF));
            assert_eq!(
                s.sys_sendmmsg(999, 0.into(), 0, 0).await,
                Err(LxError::EBADF)
            );
            let rx = s.sys_socket(AF_UNIX, SOCK_DGRAM, 0).unwrap();
            assert_eq!(s.sys_bind(rx, addr.into(), 200).await, Err(LxError::EINVAL));
            assert_eq!(s.sys_bind(rx, addr.into(), len).await, Ok(0));
            let tx = s.sys_socket(AF_UNIX, SOCK_DGRAM, 0).unwrap();
            assert_eq!(s.sys_connect(tx, addr.into(), len).await, Ok(0));
            let unspec = [0u8; 16];
            let unspec_at = unspec.as_ptr() as usize;
            assert_eq!(s.sys_connect(tx, unspec_at.into(), 16).await, Ok(0));
            let msg = *b"x";
            let msg_at = msg.as_ptr() as usize;
            assert_eq!(
                s.sys_write(tx.into(), msg_at.into(), 1).await,
                Err(LxError::ENOTCONN)
            );
            let stream = s.sys_socket(AF_UNIX, SOCK_STREAM, 0).unwrap();
            assert_eq!(
                s.sys_connect(stream, unspec_at.into(), 16).await,
                Err(LxError::EINVAL)
            );
        })
    });
}
