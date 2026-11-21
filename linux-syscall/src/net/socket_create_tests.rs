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
