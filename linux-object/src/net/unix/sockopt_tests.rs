//! The `SOL_SOCKET` options a unix socket keeps, as Linux's `sock_setsockopt`
//! keeps them for every family: `SO_REUSEADDR` reads back what was set, and
//! `SO_RCVTIMEO` / `SO_SNDTIMEO` end a read, an `accept` or a write that
//! found no room with `EAGAIN` once they run out (`sock_rcvtimeo`,
//! `sock_sndtimeo`), where they used to be taken and forgotten and the wait
//! went on for good.

extern crate std;

use super::*;
use core::time::Duration;

const SOL_SOCKET: usize = 1;
const SO_REUSEADDR: usize = 2;
const SO_RCVTIMEO: usize = 20;
const SO_SNDTIMEO: usize = 21;

fn timeval(sec: i64, usec: i64) -> [u8; 16] {
    let mut tv = [0u8; 16];
    tv[..8].copy_from_slice(&sec.to_ne_bytes());
    tv[8..].copy_from_slice(&usec.to_ne_bytes());
    tv
}

fn pair() -> (Arc<UnixSocketState>, Arc<UnixSocketState>) {
    let a = UnixSocketState::new();
    let b = UnixSocketState::new();
    UnixSocketState::connect_pair(&a, &b);
    (a, b)
}

/// `SO_KEEPALIVE` and `SO_BROADCAST` are kept as on any socket, and
/// `SO_REUSEPORT` on is `EOPNOTSUPP` for one that is not inet (off is fine).
#[test]
fn the_other_flags_are_kept_and_reuseport_is_inets() {
    let (a, _b) = pair();
    for (opt, get) in [
        (9usize, Socket::so_keepalive as fn(&UnixSocketState) -> bool),
        (6, Socket::so_broadcast),
    ] {
        assert!(!get(&a), "{}", opt);
        assert_eq!(
            Socket::setsockopt(&*a, SOL_SOCKET, opt, &1i32.to_ne_bytes()),
            Ok(0)
        );
        assert!(get(&a), "{}", opt);
        assert_eq!(
            Socket::setsockopt(&*a, SOL_SOCKET, opt, &0i32.to_ne_bytes()),
            Ok(0)
        );
        assert!(!get(&a), "{}", opt);
    }
    assert_eq!(
        Socket::setsockopt(&*a, SOL_SOCKET, 15, &1i32.to_ne_bytes()),
        Err(LxError::EOPNOTSUPP)
    );
    assert_eq!(
        Socket::setsockopt(&*a, SOL_SOCKET, 15, &0i32.to_ne_bytes()),
        Ok(0)
    );
    assert!(!Socket::so_reuseport(&*a));
}

/// `SO_REUSEADDR` reads back what was set, per end, and off by default.
#[test]
fn reuseaddr_reads_back_what_was_set_on_that_end() {
    let (a, b) = pair();
    assert!(!Socket::so_reuseaddr(&*a));
    assert_eq!(
        Socket::setsockopt(&*a, SOL_SOCKET, SO_REUSEADDR, &1i32.to_ne_bytes()),
        Ok(0)
    );
    assert!(Socket::so_reuseaddr(&*a));
    assert!(!Socket::so_reuseaddr(&*b), "the peer never asked");
    assert_eq!(
        Socket::setsockopt(&*a, SOL_SOCKET, SO_REUSEADDR, &0i32.to_ne_bytes()),
        Ok(0)
    );
    assert!(!Socket::so_reuseaddr(&*a));
    // An int, or EINVAL, as before.
    assert_eq!(
        Socket::setsockopt(&*a, SOL_SOCKET, SO_REUSEADDR, &[1u8]),
        Err(LxError::EINVAL)
    );
}

/// The two timeouts read back as the `struct timeval` that was set, and a
/// `tv_usec` outside `0..1000000` is `EDOM` (`sock_set_timeout`).
#[test]
fn the_timeouts_read_back_as_set_and_a_bad_usec_is_edom() {
    let (a, _b) = pair();
    assert_eq!(Socket::so_rcvtimeo(&*a), [0; 16]);
    assert_eq!(Socket::so_sndtimeo(&*a), [0; 16]);
    let tv = timeval(2, 500_000);
    assert_eq!(Socket::setsockopt(&*a, SOL_SOCKET, SO_RCVTIMEO, &tv), Ok(0));
    assert_eq!(Socket::so_rcvtimeo(&*a), tv);
    assert_eq!(Socket::so_sndtimeo(&*a), [0; 16], "the other one is apart");
    let tv2 = timeval(0, 7);
    assert_eq!(
        Socket::setsockopt(&*a, SOL_SOCKET, SO_SNDTIMEO, &tv2),
        Ok(0)
    );
    assert_eq!(Socket::so_sndtimeo(&*a), tv2);
    assert_eq!(
        Socket::setsockopt(&*a, SOL_SOCKET, SO_RCVTIMEO, &timeval(1, 1_000_000)),
        Err(LxError::EDOM)
    );
    assert_eq!(
        Socket::so_rcvtimeo(&*a),
        tv,
        "a refused one changes nothing"
    );
}

/// A read with nothing coming ends with `EAGAIN` once `SO_RCVTIMEO` runs
/// out, not before, and data that is there is read whatever the timeout.
#[test]
fn a_read_with_nothing_coming_ends_when_its_timeout_does() {
    let (a, b) = pair();
    Socket::setsockopt(&*a, SOL_SOCKET, SO_RCVTIMEO, &timeval(0, 50_000)).unwrap();
    let started = std::time::Instant::now();
    let mut buf = [0u8; 4];
    let (r, _) = async_std::task::block_on(Socket::read(&*a, &mut buf));
    let took = started.elapsed();
    assert_eq!(r, Err(LxError::EAGAIN));
    assert!(
        took >= Duration::from_millis(50),
        "gave up after {:?}",
        took
    );
    assert!(took < Duration::from_secs(5), "waited {:?}", took);

    Socket::write(&*b, b"hi", None).unwrap();
    let (r, _) = async_std::task::block_on(Socket::read(&*a, &mut buf));
    assert_eq!(r, Ok(2));
}

/// An `accept` with no client ends with `EAGAIN` once `SO_RCVTIMEO` runs
/// out (`inet_csk_accept` and `unix_accept` both wait `sock_rcvtimeo`).
#[test]
fn an_accept_with_no_client_ends_when_its_timeout_does() {
    let l = UnixSocketState::new();
    Socket::bind(&*l, Endpoint::Unix(String::from("/tmp/timeo-accept.sock"))).unwrap();
    Socket::listen(&*l).unwrap();
    Socket::setsockopt(&*l, SOL_SOCKET, SO_RCVTIMEO, &timeval(0, 30_000)).unwrap();
    let started = std::time::Instant::now();
    let r = async_std::task::block_on(Socket::accept(&*l)).map(|_| ());
    let took = started.elapsed();
    assert_eq!(r.err(), Some(LxError::EAGAIN));
    assert!(
        took >= Duration::from_millis(30),
        "gave up after {:?}",
        took
    );
}

/// A write that found the peer's queue full waits for room until
/// `SO_SNDTIMEO` runs out, then the wait answers `EAGAIN`.
#[test]
fn a_write_wait_for_room_ends_when_its_timeout_does() {
    let (a, _b) = pair();
    let chunk = alloc::vec![7u8; 1 << 20];
    while Socket::write(&*a, &chunk, None).is_ok() {}
    Socket::setsockopt(&*a, SOL_SOCKET, SO_SNDTIMEO, &timeval(0, 30_000)).unwrap();
    let started = std::time::Instant::now();
    let r = async_std::task::block_on(FileLike::async_poll(&*a, PollEvents::OUT));
    let took = started.elapsed();
    assert_eq!(r.err(), Some(LxError::EAGAIN));
    assert!(
        took >= Duration::from_millis(30),
        "gave up after {:?}",
        took
    );
}

/// With no timeout set the waits are what they were: a wait for room that
/// gets it resolves with the socket writable.
#[test]
fn with_no_timeout_a_wait_for_room_still_ends_on_room() {
    let (a, b) = pair();
    let chunk = alloc::vec![7u8; 1 << 20];
    while Socket::write(&*a, &chunk, None).is_ok() {}
    let drain = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut buf = alloc::vec![0u8; 1 << 16];
        let _ = async_std::task::block_on(Socket::read(&*b, &mut buf));
        b
    });
    let r = async_std::task::block_on(FileLike::async_poll(&*a, PollEvents::OUT));
    assert!(r.unwrap().write);
    drop(drain.join().unwrap());
}
