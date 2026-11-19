//! The kernel's side against a server that is a thread: an echo device,
//! each open with its own queue of what was written to it.

extern crate std;

use super::wire::{Iov, Msg, IOCTL_FETCH, MSG, OP_READY, SLOTS, SLOT_LEN};
use super::*;
use alloc::vec;
use core::convert::TryInto;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use zircon_object::ZxError;

/// `_IOWR('T', 1, [u8; 16])`: the 16 bytes come back reversed, and the
/// call returns 7.
const REVERSE: u32 = 0xc010_5401;
/// `_IO('T', 2)`: returns its argument plus one.
const PLUS_ONE: u32 = 0x5402;
/// `_IOW('T', 3, {addr: u64, len: u64})`: the sum of the `len` bytes at
/// `addr`, as a `u64` over the argument. The server asks for them.
const SUM: u32 = 0x4010_5403;
/// `_IO('T', 4)`: `EAGAIN` the first time, and the open is said to be
/// writable a little after; 5 the next.
const LATER: u32 = 0x5404;

struct Echo {
    node: Arc<ServedNode>,
    released: Arc<AtomicUsize>,
    server: Option<thread::JoinHandle<ZxError>>,
    /// The server's end, to be closed by [`Echo::kill`].
    servers: Arc<std::sync::Mutex<Option<Arc<Fifo>>>>,
}

fn send(fifo: &Fifo, m: Msg) {
    fifo.write(MSG, &m.encode(), 1).unwrap();
}

/// A server whose opens are echo queues. `quiet` opens are said to be
/// readable before the open's answer goes, as a server may.
fn echo() -> Echo {
    let (ours, theirs) = Fifo::create(SLOTS * 2, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let link = Arc::new(Link::new(ours, window.clone()).unwrap());
    let node = ServedNode::new(link, "test/echo0", rcore_fs::vfs::make_rdev(240, 1), 0o660);
    let released = Arc::new(AtomicUsize::new(0));
    let servers = Arc::new(std::sync::Mutex::new(Some(theirs.clone())));
    let rel = released.clone();
    let server = thread::spawn(move || {
        let mut queues: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let mut next = 7u32;
        let mut later = 0;
        loop {
            let mut m = [0u8; MSG];
            match theirs.read(MSG, &mut m, 1) {
                Ok(_) => {}
                Err(ZxError::SHOULD_WAIT) => {
                    thread::yield_now();
                    continue;
                }
                Err(e) => return e,
            }
            let req = Msg::decode(&m);
            let slot = (req.reqid as usize & (SLOTS - 1)) * SLOT_LEN;
            let mut reply = Msg {
                op: req.op,
                reqid: req.reqid,
                fh: req.fh,
                ..Msg::default()
            };
            let status: i64 = match req.op {
                OP_OPEN => {
                    next += 1;
                    queues.insert(next, Vec::new());
                    reply.fh = next;
                    reply.a = POLLOUT;
                    0
                }
                OP_RELEASE => {
                    queues.remove(&req.fh);
                    rel.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
                OP_WRITE => {
                    let mut got = vec![0u8; req.len as usize];
                    window.read(slot, &mut got).unwrap();
                    let q = queues.get_mut(&req.fh).unwrap();
                    q.extend_from_slice(&got);
                    // Readable, said before the answer.
                    send(
                        &theirs,
                        Msg {
                            op: OP_READY,
                            fh: req.fh,
                            a: POLLIN | POLLOUT,
                            ..Msg::default()
                        },
                    );
                    got.len() as i64
                }
                OP_READ => {
                    let q = queues.get_mut(&req.fh).unwrap();
                    if q.is_empty() {
                        -11
                    } else {
                        let n = q.len().min(req.len as usize);
                        let out: Vec<u8> = q.drain(..n).collect();
                        window.write(slot, &out).unwrap();
                        reply.len = n as u32;
                        if q.is_empty() {
                            send(
                                &theirs,
                                Msg {
                                    op: OP_READY,
                                    fh: req.fh,
                                    a: POLLOUT,
                                    ..Msg::default()
                                },
                            );
                        }
                        n as i64
                    }
                }
                OP_IOCTL if req.a == REVERSE as u64 => {
                    let mut got = vec![0u8; req.len as usize];
                    window.read(slot, &mut got).unwrap();
                    got.reverse();
                    window.write(slot, &got).unwrap();
                    reply.len = got.len() as u32;
                    7
                }
                OP_IOCTL if req.a == PLUS_ONE as u64 => req.b as i64 + 1,
                OP_IOCTL if req.a == SUM as u64 => {
                    let mut got = [0u8; 16];
                    window.read(slot, &mut got).unwrap();
                    let iov = Iov {
                        ins: vec![(
                            u64::from_ne_bytes(got[..8].try_into().unwrap()),
                            u64::from_ne_bytes(got[8..].try_into().unwrap()),
                        )],
                        outs: vec![(req.b, 8)],
                    }
                    .encode();
                    window.write(slot, &iov).unwrap();
                    reply.len = iov.len() as u32;
                    reply.a = IOCTL_FETCH;
                    0
                }
                super::wire::OP_IOCTL_IOV if req.a == SUM as u64 => {
                    let mut got = vec![0u8; req.len as usize];
                    window.read(slot, &mut got).unwrap();
                    let iov = Iov::decode(&got).unwrap();
                    let at = Iov::len_of(1, 1);
                    let sum: u64 = got[at..at + iov.ins[0].1 as usize]
                        .iter()
                        .map(|b| *b as u64)
                        .sum();
                    window.write(slot, &sum.to_ne_bytes()).unwrap();
                    reply.len = 8;
                    0
                }
                OP_IOCTL if req.a as u32 == LATER => {
                    later += 1;
                    // The open that waits is said to.
                    assert_eq!(req.a & super::wire::IOCTL_WAITS != 0, later > 2);
                    if later % 2 == 1 {
                        reply.b = (-11i64) as u64;
                        send(&theirs, reply);
                        thread::sleep(std::time::Duration::from_millis(20));
                        send(
                            &theirs,
                            Msg {
                                op: OP_READY,
                                fh: req.fh,
                                a: POLLOUT,
                                ..Msg::default()
                            },
                        );
                        continue;
                    }
                    5
                }
                OP_IOCTL => -25,
                _ => -22,
            };
            reply.b = status as u64;
            send(&theirs, reply);
        }
    });
    Echo {
        node,
        released,
        server: Some(server),
        servers,
    }
}

impl Echo {
    /// The server dies: its end of the FIFO closes.
    fn kill(&mut self) {
        self.servers.lock().unwrap().take();
        self.node.server_left();
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.node.server_left();
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn ready(open: &Arc<dyn INode>) -> (bool, bool, bool, bool) {
    let p = open.poll().unwrap();
    (p.read, p.write, p.error, p.hangup)
}

#[test]
fn an_open_is_the_servers_and_starts_as_it_says() {
    let echo = echo();
    let open = echo.node.open_client(2).unwrap();
    assert_eq!(ready(&open), (false, true, false, false));
    let meta = open.metadata().unwrap();
    assert_eq!(meta.type_, FileType::CharDevice);
    assert_eq!(meta.rdev, rcore_fs::vfs::make_rdev(240, 1));
    assert_eq!(meta.mode, 0o660);
}

#[test]
fn a_read_with_nothing_there_is_eagain_and_what_was_written_reads_back() {
    let echo = echo();
    let open = echo.node.open_client(2).unwrap();
    let mut buf = [0u8; 16];
    assert!(matches!(open.read_at(0, &mut buf), Err(FsError::Again)));
    assert_eq!(open.write_at(0, b"hola").unwrap(), 4);
    // The readiness came before the answer: the open is readable now.
    assert_eq!(ready(&open), (true, true, false, false));
    assert_eq!(open.read_at(0, &mut buf).unwrap(), 4);
    assert_eq!(&buf[..4], b"hola");
    assert_eq!(ready(&open), (false, true, false, false));
}

#[test]
fn each_open_has_its_own_readiness() {
    let echo = echo();
    let one = echo.node.open_client(2).unwrap();
    let two = echo.node.open_client(2).unwrap();
    one.write_at(0, b"x").unwrap();
    assert!(ready(&one).0);
    assert!(!ready(&two).0);
}

#[test]
fn a_write_longer_than_a_slot_goes_a_slot_at_a_time() {
    let echo = echo();
    let open = echo.node.open_client(2).unwrap();
    let big: Vec<u8> = (0..SLOT_LEN * 2 + 100).map(|i| (i % 251) as u8).collect();
    assert_eq!(open.write_at(0, &big).unwrap(), big.len());
    let mut back = Vec::new();
    let mut buf = vec![0u8; SLOT_LEN * 3];
    while back.len() < big.len() {
        let n = open.read_at(0, &mut buf).unwrap();
        assert!(n <= SLOT_LEN);
        back.extend_from_slice(&buf[..n]);
    }
    assert_eq!(back, big);
}

#[test]
fn an_ioctl_carries_what_its_number_says_and_returns_the_servers_answer() {
    let echo = echo();
    let open = echo.node.open_client(2).unwrap();
    let open = open.downcast_ref::<ServedOpen>().unwrap();
    let mut arg: [u8; 16] = core::array::from_fn(|i| i as u8);
    assert_eq!(open.ioctl(REVERSE, arg.as_mut_ptr() as usize).unwrap(), 7);
    let want: [u8; 16] = core::array::from_fn(|i| 15 - i as u8);
    assert_eq!(arg, want);
    assert_eq!(open.ioctl(PLUS_ONE, 41).unwrap(), 42);
    assert_eq!(open.ioctl(0x5403, 0), Err(LxError::ENOTTY));
}

#[test]
fn an_ioctl_whose_argument_points_further_reads_and_writes_what_the_server_asks() {
    let echo = echo();
    let open = echo.node.open_client(2).unwrap();
    let open = open.downcast_ref::<ServedOpen>().unwrap();
    let samples: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let want: u64 = samples.iter().map(|b| *b as u64).sum();
    let mut arg = [0u64; 2];
    arg[0] = samples.as_ptr() as u64;
    arg[1] = samples.len() as u64;
    assert_eq!(open.ioctl(SUM, arg.as_mut_ptr() as usize).unwrap(), 0);
    assert_eq!(arg[0], want);
}

#[test]
fn an_ioctl_that_cannot_be_done_yet_waits_for_the_server_on_an_open_that_waits() {
    let echo = echo();
    let open = echo.node.open_client(2).unwrap();
    let open = open.downcast_ref::<ServedOpen>().unwrap();
    // One that does not wait is told.
    assert_eq!(open.ioctl(LATER, 0), Err(LxError::EAGAIN));
    assert_eq!(open.ioctl(LATER, 0).unwrap(), 5);
    // One that waits is asked again once the server has said something,
    // which the kernel reads as it does once the FIFO is readable.
    let node = echo.node.clone();
    let done = Arc::new(core::sync::atomic::AtomicBool::new(false));
    let stop = done.clone();
    let looker = thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            node.look();
            thread::sleep(std::time::Duration::from_millis(1));
        }
    });
    let start = std::time::Instant::now();
    let got = async_std::task::block_on(open.ioctl_waiting(LATER, 0));
    done.store(true, Ordering::SeqCst);
    looker.join().unwrap();
    assert_eq!(got.unwrap(), 5);
    assert!(start.elapsed() >= std::time::Duration::from_millis(15));
}

#[test]
fn a_closed_open_is_told_to_the_server() {
    let echo = echo();
    let open = echo.node.open_client(0).unwrap();
    drop(open);
    // Told, not asked: it arrives a moment later.
    for _ in 0..1000 {
        if echo.released.load(Ordering::SeqCst) == 1 {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("the server was not told of the close");
}

#[test]
fn readiness_said_before_the_open_is_known_is_kept_for_it() {
    let echo = echo();
    // Fh 8 is the next open's; say it is readable before it is opened.
    echo.node.0.ready(8, POLLIN);
    let open = echo.node.open_client(2).unwrap();
    assert_eq!(ready(&open), (true, false, false, false));
}

#[test]
fn when_the_server_goes_its_opens_hang_up_and_ask_nothing() {
    let mut echo = echo();
    let open = echo.node.open_client(2).unwrap();
    echo.kill();
    assert_eq!(ready(&open), (false, false, true, true));
    let mut buf = [0u8; 4];
    assert!(matches!(open.read_at(0, &mut buf), Err(FsError::NoDevice)));
    assert!(matches!(open.write_at(0, b"x"), Err(FsError::NoDevice)));
    assert_eq!(
        echo.node.open_client(2).err(),
        Some(LxError::ENODEV),
        "nobody to open it"
    );
}

#[test]
fn a_name_is_a_few_plain_names() {
    for good in [
        "input/event4",
        "dsp",
        "snd/pcmC0D0p",
        "a/b/c/d",
        "bus-1_x.y",
    ] {
        assert!(valid_name(good), "{}", good);
    }
    for bad in [
        "",
        "/input",
        "input/",
        "a//b",
        "../x",
        "a/./b",
        "a/b/c/d/e",
        "sp ace",
        "ñ",
    ] {
        assert!(!valid_name(bad), "{}", bad);
    }
}
