//! `setsockopt(2)` then `getsockopt(2)`: what each option answers before it
//! is set, after, and on a socket of a family that does not have it, as
//! `sock_getsockopt`, `do_tcp_getsockopt` and `do_ip_getsockopt` answer.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::{boxed::Box, vec};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const AF_UNIX: usize = 1;
const AF_INET: usize = 2;
const AF_INET6: usize = 10;
const AF_NETLINK: usize = 16;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOCK_RAW: usize = 3;
const IPPROTO_IP: usize = 0;
const IPPROTO_TCP: usize = 6;
const IPPROTO_UDP: usize = 17;

const SO_REUSEADDR: usize = 2;
const SO_ERROR: usize = 4;
const SO_BROADCAST: usize = 6;
const SO_SNDBUF: usize = 7;
const SO_RCVBUF: usize = 8;
const SO_KEEPALIVE: usize = 9;
const SO_LINGER: usize = 13;
const SO_REUSEPORT: usize = 15;
const SO_RCVTIMEO: usize = 20;
const SO_SNDTIMEO: usize = 21;
const SO_ACCEPTCONN: usize = 30;
const SO_PROTOCOL: usize = 38;
const SO_DOMAIN: usize = 39;

const TCP_NODELAY: usize = 1;
const TCP_KEEPIDLE: usize = 4;
const TCP_KEEPINTVL: usize = 5;
const TCP_KEEPCNT: usize = 6;
const TCP_CONGESTION: usize = 13;

const IP_TOS: usize = 1;
const IP_TTL: usize = 2;
const IP_HDRINCL: usize = 3;
const IP_MULTICAST_IF: usize = 32;
const IP_MULTICAST_TTL: usize = 33;
const IP_MULTICAST_LOOP: usize = 34;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "sockopts",
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

/// The answer of `getsockopt`, as many bytes as it said it wrote.
fn get(s: &mut Syscall, fd: usize, level: usize, name: usize) -> LxResult<Vec<u8>> {
    let mut val = [0u8; 64];
    let mut len: u32 = val.len() as u32;
    s.sys_getsockopt(
        fd,
        level,
        name,
        (val.as_mut_ptr() as usize).into(),
        (&mut len as *mut u32 as usize).into(),
    )?;
    Ok(val[..len as usize].to_vec())
}

fn get_int(s: &mut Syscall, fd: usize, level: usize, name: usize) -> LxResult<u32> {
    let v = get(s, fd, level, name)?;
    assert_eq!(v.len(), 4, "an int is four bytes");
    Ok(u32::from_ne_bytes([v[0], v[1], v[2], v[3]]))
}

fn set(s: &mut Syscall, fd: usize, level: usize, name: usize, val: &[u8]) -> SysResult {
    s.sys_setsockopt(fd, level, name, (val.as_ptr() as usize).into(), val.len())
}

fn set_int(s: &mut Syscall, fd: usize, level: usize, name: usize, val: u32) -> SysResult {
    set(s, fd, level, name, &val.to_ne_bytes())
}

/// A `struct timeval` of this kernel's width.
fn timeval(sec: i64, usec: i64) -> [u8; 16] {
    let mut tv = [0u8; 16];
    tv[..8].copy_from_slice(&sec.to_ne_bytes());
    tv[8..].copy_from_slice(&usec.to_ne_bytes());
    tv
}

/// Every flag of the socket level is 0 until it is set, reads back what
/// was set, and reads 0 again once it is cleared: `sock_valbool_flag`.
#[test]
fn a_flag_of_the_socket_level_reads_back_what_was_set() {
    let proc = a_process(43_300);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            // An AF_UNIX socket takes them and keeps none.
            for (domain, ty) in [(AF_INET, SOCK_STREAM), (AF_INET, SOCK_DGRAM)] {
                let fd = s.sys_socket(domain, ty, 0).unwrap();
                for name in [SO_REUSEADDR, SO_KEEPALIVE, SO_BROADCAST, SO_REUSEPORT] {
                    // A served TCP socket keeps only SO_REUSEADDR of these.
                    if ty == SOCK_STREAM && name != SO_REUSEADDR {
                        continue;
                    }
                    let what = (domain, ty, name);
                    assert_eq!(get_int(&mut s, fd, SOL_SOCKET, name), Ok(0), "{what:?}");
                    assert_eq!(set_int(&mut s, fd, SOL_SOCKET, name, 7), Ok(0), "{what:?}");
                    assert_eq!(
                        get_int(&mut s, fd, SOL_SOCKET, name),
                        Ok(1),
                        "{what:?}: a bool"
                    );
                    assert_eq!(set_int(&mut s, fd, SOL_SOCKET, name, 0), Ok(0));
                    assert_eq!(get_int(&mut s, fd, SOL_SOCKET, name), Ok(0), "{what:?}");
                }
            }
        })
    });
}

/// `SO_TYPE`, `SO_DOMAIN` and `SO_PROTOCOL` say what `socket(2)` was asked:
/// the protocol the family chose when it was asked for 0.
#[test]
fn a_socket_says_its_type_domain_and_protocol() {
    let proc = a_process(43_301);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            for (domain, ty, proto, want_proto) in [
                (AF_INET, SOCK_STREAM, IPPROTO_IP, IPPROTO_TCP),
                (AF_INET, SOCK_STREAM, IPPROTO_TCP, IPPROTO_TCP),
                (AF_INET, SOCK_DGRAM, IPPROTO_IP, IPPROTO_UDP),
                (AF_INET6, SOCK_DGRAM, IPPROTO_UDP, IPPROTO_UDP),
                (AF_INET6, SOCK_STREAM, IPPROTO_IP, IPPROTO_TCP),
                (AF_UNIX, SOCK_DGRAM, 0, 0),
            ] {
                let fd = s.sys_socket(domain, ty, proto).unwrap();
                let what = (domain, ty, proto);
                assert_eq!(
                    get_int(&mut s, fd, SOL_SOCKET, SO_TYPE),
                    Ok(ty as u32),
                    "{what:?}"
                );
                assert_eq!(
                    get_int(&mut s, fd, SOL_SOCKET, SO_DOMAIN),
                    Ok(domain as u32),
                    "{what:?}"
                );
                assert_eq!(
                    get_int(&mut s, fd, SOL_SOCKET, SO_PROTOCOL),
                    Ok(want_proto as u32),
                    "{what:?}"
                );
            }
            let fd = s.sys_socket(AF_NETLINK, SOCK_RAW, 0).unwrap();
            assert_eq!(
                get_int(&mut s, fd, SOL_SOCKET, SO_TYPE),
                Ok(SOCK_RAW as u32)
            );
            assert_eq!(
                get_int(&mut s, fd, SOL_SOCKET, SO_DOMAIN),
                Ok(AF_NETLINK as u32)
            );
        })
    });
}

/// A new socket has no pending error, and `SO_ERROR` reads 0.
#[test]
fn a_new_socket_has_no_error_pending() {
    let proc = a_process(43_302);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            for (domain, ty) in [
                (AF_INET, SOCK_STREAM),
                (AF_INET, SOCK_DGRAM),
                (AF_UNIX, SOCK_STREAM),
            ] {
                let fd = s.sys_socket(domain, ty, 0).unwrap();
                assert_eq!(get_int(&mut s, fd, SOL_SOCKET, SO_ERROR), Ok(0));
            }
        })
    });
}

/// Every socket has buffers of a size, and reports one, even one with no
/// queue of its own (an AF_UNIX one answers the system's default).
#[test]
fn every_socket_reports_a_buffer_size() {
    let proc = a_process(43_303);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            for (domain, ty) in [
                (AF_INET, SOCK_STREAM),
                (AF_INET, SOCK_DGRAM),
                (AF_UNIX, SOCK_STREAM),
            ] {
                let fd = s.sys_socket(domain, ty, 0).unwrap();
                for name in [SO_SNDBUF, SO_RCVBUF] {
                    let size = get_int(&mut s, fd, SOL_SOCKET, name).unwrap();
                    assert!(size > 0, "{} {} {}: {}", domain, ty, name, size);
                }
            }
        })
    });
}

/// `SO_LINGER` is a `struct linger` of two ints, off with no time until it
/// is set, and reads back what was set.
#[test]
fn linger_is_off_until_it_is_set() {
    let proc = a_process(43_304);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            let fd = s.sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap();
            assert_eq!(get(&mut s, fd, SOL_SOCKET, SO_LINGER), Ok(vec![0; 8]));
            let mut on = [0u8; 8];
            on[..4].copy_from_slice(&1i32.to_ne_bytes());
            on[4..].copy_from_slice(&5i32.to_ne_bytes());
            assert_eq!(set(&mut s, fd, SOL_SOCKET, SO_LINGER, &on), Ok(0));
            assert_eq!(get(&mut s, fd, SOL_SOCKET, SO_LINGER), Ok(on.to_vec()));
            // `sock_setsockopt`: shorter than a `struct linger` is EINVAL.
            assert_eq!(
                set(&mut s, fd, SOL_SOCKET, SO_LINGER, &on[..4]),
                Err(LxError::EINVAL)
            );
        })
    });
}

/// `SO_RCVTIMEO` and `SO_SNDTIMEO` are zero (wait for ever) until set, and
/// read back the `timeval` that was set.
#[test]
fn a_timeout_reads_back_the_timeval_that_was_set() {
    let proc = a_process(43_305);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            // An AF_UNIX socket takes both and keeps neither: its reads and
            // writes wait for ever whatever they were told.
            for (domain, ty) in [(AF_INET, SOCK_STREAM), (AF_INET, SOCK_DGRAM)] {
                let fd = s.sys_socket(domain, ty, 0).unwrap();
                for name in [SO_RCVTIMEO, SO_SNDTIMEO] {
                    let what = (domain, ty, name);
                    assert_eq!(
                        get(&mut s, fd, SOL_SOCKET, name),
                        Ok(vec![0; 16]),
                        "{what:?}"
                    );
                    let tv = timeval(2, 500_000);
                    assert_eq!(set(&mut s, fd, SOL_SOCKET, name, &tv), Ok(0), "{what:?}");
                    assert_eq!(
                        get(&mut s, fd, SOL_SOCKET, name),
                        Ok(tv.to_vec()),
                        "{what:?}"
                    );
                    // `sock_set_timeout`: a usec of a second or more is EDOM.
                    let bad = timeval(1, 1_000_000);
                    assert_eq!(
                        set(&mut s, fd, SOL_SOCKET, name, &bad),
                        Err(LxError::EDOM),
                        "{what:?}"
                    );
                }
            }
        })
    });
}

/// `SO_ACCEPTCONN` is 1 once `listen(2)` made the socket passive, and 0
/// before.
#[test]
fn acceptconn_says_whether_the_socket_listens() {
    let proc = a_process(43_306);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            let fd = s.sys_socket(AF_UNIX, SOCK_STREAM, 0).unwrap();
            assert_eq!(get_int(&mut s, fd, SOL_SOCKET, SO_ACCEPTCONN), Ok(0));
            let mut sun = [0u8; 110];
            sun[..2].copy_from_slice(&1u16.to_ne_bytes());
            // An abstract name: no file is made for it.
            let name = b"\0eclipse-acceptconn-test";
            sun[2..2 + name.len()].copy_from_slice(name);
            assert_eq!(
                s.sys_bind(fd, (sun.as_ptr() as usize).into(), 2 + name.len())
                    .await,
                Ok(0)
            );
            assert_eq!(s.sys_listen(fd, 4), Ok(0));
            assert_eq!(get_int(&mut s, fd, SOL_SOCKET, SO_ACCEPTCONN), Ok(1));
        })
    });
}

/// The TCP options, on a TCP socket: `TCP_NODELAY` off, the keepalive
/// times at `net.ipv4.tcp_keepalive_*` (7200 s, 75 s, 9 probes), and a
/// congestion control named with its NUL.
#[test]
fn the_tcp_options_of_a_tcp_socket() {
    let proc = a_process(43_307);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            let fd = s.sys_socket(AF_INET, SOCK_STREAM, 0).unwrap();
            assert_eq!(get_int(&mut s, fd, IPPROTO_TCP, TCP_NODELAY), Ok(0));
            assert_eq!(get_int(&mut s, fd, IPPROTO_TCP, TCP_KEEPIDLE), Ok(7200));
            assert_eq!(get_int(&mut s, fd, IPPROTO_TCP, TCP_KEEPINTVL), Ok(75));
            assert_eq!(get_int(&mut s, fd, IPPROTO_TCP, TCP_KEEPCNT), Ok(9));
            let cca = get(&mut s, fd, IPPROTO_TCP, TCP_CONGESTION).unwrap();
            assert_eq!(cca.last(), Some(&0), "a C string");
            assert!(cca.len() > 1);
        })
    });
}

/// `do_tcp_getsockopt` is TCP's: a datagram socket and an AF_UNIX stream
/// one have no TCP options, and an option TCP has not is `ENOPROTOOPT`.
#[test]
fn a_socket_that_is_not_tcp_has_no_tcp_options() {
    let proc = a_process(43_308);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            for (domain, ty) in [(AF_INET, SOCK_DGRAM), (AF_UNIX, SOCK_STREAM)] {
                let fd = s.sys_socket(domain, ty, 0).unwrap();
                assert_eq!(
                    get(&mut s, fd, IPPROTO_TCP, TCP_NODELAY),
                    Err(LxError::ENOPROTOOPT),
                    "{domain} {ty}"
                );
            }
            let fd = s.sys_socket(AF_INET, SOCK_STREAM, 0).unwrap();
            assert_eq!(get(&mut s, fd, IPPROTO_TCP, 999), Err(LxError::ENOPROTOOPT));
        })
    });
}

/// The IP options of an inet socket, at Linux's defaults: TOS 0, TTL 64
/// (`net.ipv4.ip_default_ttl`), multicast TTL 1 and loop on, no multicast
/// interface; and they read back what was set.
#[test]
fn the_ip_options_of_an_inet_socket() {
    let proc = a_process(43_309);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            for ty in [SOCK_STREAM, SOCK_DGRAM] {
                let fd = s.sys_socket(AF_INET, ty, 0).unwrap();
                assert_eq!(get_int(&mut s, fd, IPPROTO_IP, IP_TOS), Ok(0), "{ty}");
                assert_eq!(get_int(&mut s, fd, IPPROTO_IP, IP_TTL), Ok(64), "{ty}");
                assert_eq!(
                    get_int(&mut s, fd, IPPROTO_IP, IP_MULTICAST_TTL),
                    Ok(1),
                    "{ty}"
                );
                assert_eq!(
                    get_int(&mut s, fd, IPPROTO_IP, IP_MULTICAST_LOOP),
                    Ok(1),
                    "{ty}"
                );
                assert_eq!(
                    get_int(&mut s, fd, IPPROTO_IP, IP_MULTICAST_IF),
                    Ok(0),
                    "{ty}"
                );
                if ty == SOCK_STREAM {
                    continue;
                }
                assert_eq!(set_int(&mut s, fd, IPPROTO_IP, IP_TTL, 12), Ok(0));
                assert_eq!(get_int(&mut s, fd, IPPROTO_IP, IP_TTL), Ok(12), "{ty}");
                assert_eq!(set_int(&mut s, fd, IPPROTO_IP, IP_TOS, 0x10), Ok(0));
                assert_eq!(get_int(&mut s, fd, IPPROTO_IP, IP_TOS), Ok(0x10), "{ty}");
                // `IP_HDRINCL` is of a raw socket only.
                assert_eq!(
                    get(&mut s, fd, IPPROTO_IP, IP_HDRINCL),
                    Err(LxError::ENOPROTOOPT),
                    "{ty}"
                );
            }
        })
    });
}

/// `do_ip_getsockopt` is inet's: an AF_UNIX socket has no IP options.
#[test]
fn a_socket_that_is_not_inet_has_no_ip_options() {
    let proc = a_process(43_310);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            let fd = s.sys_socket(AF_UNIX, SOCK_DGRAM, 0).unwrap();
            for name in [IP_TOS, IP_TTL, IP_MULTICAST_TTL] {
                assert_eq!(get(&mut s, fd, IPPROTO_IP, name), Err(LxError::ENOPROTOOPT));
            }
            let fd = s.sys_socket(AF_INET, SOCK_DGRAM, 0).unwrap();
            assert_eq!(get(&mut s, fd, IPPROTO_IP, 999), Err(LxError::ENOPROTOOPT));
            assert_eq!(get(&mut s, fd, SOL_SOCKET, 999), Err(LxError::ENOPROTOOPT));
        })
    });
}

/// Neither call is anything but `EBADF` on a descriptor that is not open,
/// and `ENOTSOCK` on one that is not a socket.
#[test]
fn an_option_of_what_is_not_a_socket() {
    let proc = a_process(43_311);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut s = own(sys);
            assert_eq!(get(&mut s, 77, SOL_SOCKET, SO_TYPE), Err(LxError::EBADF));
            assert_eq!(
                set_int(&mut s, 77, SOL_SOCKET, SO_REUSEADDR, 1),
                Err(LxError::EBADF)
            );
            let mut fds = [0i32; 2];
            s.sys_pipe2((fds.as_mut_ptr() as usize).into(), 0).unwrap();
            let fd = fds[0] as usize;
            assert_eq!(get(&mut s, fd, SOL_SOCKET, SO_TYPE), Err(LxError::ENOTSOCK));
            assert_eq!(
                set_int(&mut s, fd, SOL_SOCKET, SO_REUSEADDR, 1),
                Err(LxError::ENOTSOCK)
            );
        })
    });
}
