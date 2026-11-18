//! `udrv-dev-echo serve NAME` | `udrv-dev-echo check NAME [ROUNDS]`: the
//! test of a node in `/dev` that a process serves (`eclipse_dev_serve`).
//!
//! `serve` puts `/dev/NAME` there and serves it: each open is a queue, what
//! is written to it is what it reads back, and two `ioctl`s answer
//! (`REVERSE`, `PLUS_ONE`). `check` starts a `serve` of its own and checks,
//! as a program would see them, the open, `poll`, a read with nothing there
//! and one that waits, writes, the `ioctl`s and the server's going and
//! coming back; then it times `ROUNDS` (default 2000) write-and-read turns.
//! It prints `dev-echo: ok` when everything held.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::process::{exit, Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use udrivers::served::{self, Device, Node, EAGAIN, ENOTTY, POLLIN, POLLOUT};

const NAME: &str = "udrv-dev-echo";

/// `_IOWR('T', 1, [u8; 16])`: the bytes come back reversed; returns 7.
const REVERSE: u32 = 0xc010_5401;
/// `_IO('T', 2)`: returns its argument plus one.
const PLUS_ONE: u32 = 0x5402;
/// `_IO('T', 3)`: say no more readiness of this open (to time what saying
/// it costs).
const QUIET: u32 = 0x5403;

fn die(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1);
}

#[derive(Default)]
struct Echo {
    queues: BTreeMap<u32, Vec<u8>>,
    next: u32,
    /// Opens that asked for no readiness (`QUIET`).
    quiet: std::collections::BTreeSet<u32>,
}

impl Device for Echo {
    fn open(&mut self, _flags: u64) -> Result<(u32, u64), i32> {
        self.next += 1;
        self.queues.insert(self.next, Vec::new());
        Ok((self.next, POLLOUT))
    }

    fn release(&mut self, fh: u32) {
        self.queues.remove(&fh);
        self.quiet.remove(&fh);
    }

    fn read(&mut self, node: &Node, fh: u32, _off: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let q = self.queues.get_mut(&fh).ok_or(served::EINVAL)?;
        if q.is_empty() {
            return Err(EAGAIN);
        }
        let n = q.len().min(buf.len());
        buf[..n].copy_from_slice(&q[..n]);
        q.drain(..n);
        if q.is_empty() && !self.quiet.contains(&fh) {
            node.ready(fh, POLLOUT);
        }
        Ok(n)
    }

    fn write(&mut self, node: &Node, fh: u32, _off: u64, data: &[u8]) -> Result<usize, i32> {
        let q = self.queues.get_mut(&fh).ok_or(served::EINVAL)?;
        let was_empty = q.is_empty();
        q.extend_from_slice(data);
        if was_empty && !data.is_empty() && !self.quiet.contains(&fh) {
            node.ready(fh, POLLIN | POLLOUT);
        }
        Ok(data.len())
    }

    fn ioctl(
        &mut self,
        _node: &Node,
        fh: u32,
        cmd: u32,
        arg: u64,
        data: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        match cmd {
            REVERSE => {
                data[..16].reverse();
                Ok((7, 16))
            }
            PLUS_ONE => Ok((arg as i64 + 1, 0)),
            QUIET => {
                self.quiet.insert(fh);
                Ok((0, 0))
            }
            _ => Err(ENOTTY),
        }
    }
}

fn serve(name: &str) -> ! {
    let node = Node::serve(name, 240, 0, 0o666).unwrap_or_else(|e| die(e));
    println!("{NAME}: serving /dev/{name}");
    let echo = Mutex::new(Echo::default());
    die(format!(
        "the kernel let go of /dev/{name}: {}",
        node.run(&echo)
    ))
}

fn start_server(name: &str) -> Child {
    let me = std::env::current_exe().unwrap_or_else(|e| die(e));
    let mut child = Command::new(me)
        .args(["serve", name])
        .spawn()
        .unwrap_or_else(|e| die(e));
    let path = format!("/dev/{name}");
    for _ in 0..500 {
        if std::path::Path::new(&path).exists() {
            return child;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    die(format!("{path} did not appear"))
}

fn open(path: &str, flags: i32) -> i32 {
    let c = CString::new(path).unwrap();
    // SAFETY: a NUL-terminated path.
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        die(format!("open {path}: {}", std::io::Error::last_os_error()));
    }
    fd
}

fn poll(fd: i32, timeout_ms: i32) -> i16 {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one pollfd.
    unsafe { libc::poll(&mut p, 1, timeout_ms) };
    p.revents
}

fn read(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    // SAFETY: `buf` is writable for its length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(n as usize)
    }
}

fn write(fd: i32, data: &[u8]) -> usize {
    // SAFETY: `data` is readable for its length.
    let n = unsafe { libc::write(fd, data.as_ptr() as *const _, data.len()) };
    if n < 0 {
        die(format!("write: {}", std::io::Error::last_os_error()));
    }
    n as usize
}

fn check(what: &str, ok: bool) {
    if !ok {
        die(format!("FAILED: {what}"));
    }
    println!("{NAME}: {what}: ok");
}

fn run_check(name: &str, rounds: usize) {
    let path = format!("/dev/{name}");
    let mut server = start_server(name);

    // The node.
    let c = CString::new(path.clone()).unwrap();
    // SAFETY: a zeroed stat to fill.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a path and a stat.
    let r = unsafe { libc::stat(c.as_ptr(), &mut st) };
    check(
        "the node is a character device 240:0",
        r == 0 && st.st_mode & libc::S_IFMT == libc::S_IFCHR && st.st_rdev == libc::makedev(240, 0),
    );

    let fd = open(&path, libc::O_RDWR | libc::O_NONBLOCK);
    let ev = poll(fd, 0);
    check(
        "a new open is writable and not readable",
        ev == libc::POLLOUT,
    );
    let mut buf = [0u8; 64];
    check(
        "a read with nothing there is EAGAIN",
        read(fd, &mut buf) == Err(libc::EAGAIN),
    );
    write(fd, b"hola");
    check("written, it is readable", poll(fd, 0) & libc::POLLIN != 0);
    check(
        "it reads back what was written",
        read(fd, &mut buf) == Ok(4) && &buf[..4] == b"hola",
    );
    check("read, it is not readable", poll(fd, 0) == libc::POLLOUT);

    // A read that waits, on an open of its own, woken by another open's
    // write... of its own queue: the waiter's fd is written from here.
    let waiter = open(&path, libc::O_RDWR);
    let t = std::thread::spawn(move || {
        let mut b = [0u8; 8];
        let started = Instant::now();
        let r = read(waiter, &mut b);
        (r, b, started.elapsed())
    });
    std::thread::sleep(Duration::from_millis(200));
    write(waiter, b"tarde");
    let (r, b, took) = t.join().unwrap();
    check(
        "a blocking read waits for what is written later",
        r == Ok(5) && &b[..5] == b"tarde" && took >= Duration::from_millis(150),
    );

    let mut arg: [u8; 16] = core::array::from_fn(|i| i as u8);
    // SAFETY: the 16 bytes the command carries.
    let r = unsafe { libc::ioctl(fd, REVERSE as _, arg.as_mut_ptr()) };
    let want: [u8; 16] = core::array::from_fn(|i| 15 - i as u8);
    check("an _IOWR ioctl goes both ways", r == 7 && arg == want);
    // SAFETY: a number for an argument.
    let r = unsafe { libc::ioctl(fd, PLUS_ONE as _, 41usize) };
    check("an _IO ioctl takes a number", r == 42);
    // SAFETY: as above.
    let r = unsafe { libc::ioctl(fd, 0x5404 as _, 0usize) };
    let e = std::io::Error::last_os_error().raw_os_error();
    check(
        "an unknown ioctl is ENOTTY",
        r == -1 && e == Some(libc::ENOTTY),
    );

    // The cost of a turn: a write and a read.
    let started = Instant::now();
    for i in 0..rounds {
        let b = [i as u8; 24];
        write(fd, &b);
        if read(fd, &mut buf) != Ok(24) {
            die("a timed read came back short");
        }
    }
    let us = started.elapsed().as_micros() as usize;
    println!(
        "{NAME}: {rounds} write+read turns: {us} us, {} us each",
        us / rounds.max(1)
    );

    // Its two halves.
    let started = Instant::now();
    for i in 0..rounds {
        write(fd, &[i as u8; 24]);
    }
    let w = started.elapsed().as_micros() as usize;
    let started = Instant::now();
    for _ in 0..rounds {
        if read(fd, &mut buf[..24]) != Ok(24) {
            die("a timed read came back short");
        }
    }
    let r = started.elapsed().as_micros() as usize;
    println!(
        "{NAME}: {rounds} writes: {} us each; {rounds} reads: {} us each",
        w / rounds.max(1),
        r / rounds.max(1)
    );

    // And of one request alone, an ioctl that carries nothing.
    let started = Instant::now();
    for i in 0..rounds {
        // SAFETY: a number for an argument.
        if unsafe { libc::ioctl(fd, PLUS_ONE as _, i) } != i as i32 + 1 {
            die("a timed ioctl answered wrong");
        }
    }
    let us = started.elapsed().as_micros() as usize;
    println!(
        "{NAME}: {rounds} ioctls: {us} us, {} us each",
        us / rounds.max(1)
    );

    // The same turns, with no readiness said.
    // SAFETY: a number for an argument.
    unsafe { libc::ioctl(fd, QUIET as _, 0usize) };
    let started = Instant::now();
    for i in 0..rounds {
        write(fd, &[i as u8; 24]);
        if read(fd, &mut buf) != Ok(24) {
            die("a quiet timed read came back short");
        }
    }
    let us = started.elapsed().as_micros() as usize;
    println!(
        "{NAME}: {rounds} quiet write+read turns: {} us each",
        us / rounds.max(1)
    );

    // The floor any process-to-process turn stands on: a byte there and
    // back through two pipes, with a child at the other end.
    let (mut a, mut b) = ([0i32; 2], [0i32; 2]);
    // SAFETY: two pipes, then a child that echoes until its pipe closes.
    unsafe {
        libc::pipe(a.as_mut_ptr());
        libc::pipe(b.as_mut_ptr());
        if libc::fork() == 0 {
            let mut c = [0u8; 1];
            while libc::read(a[0], c.as_mut_ptr() as *mut _, 1) == 1 {
                libc::write(b[1], c.as_ptr() as *const _, 1);
            }
            libc::_exit(0);
        }
    }
    let started = Instant::now();
    for _ in 0..rounds {
        write(a[1], b"x");
        let _ = read(b[0], &mut buf[..1]);
    }
    let us = started.elapsed().as_micros() as usize;
    println!(
        "{NAME}: {rounds} pipe ping-pongs: {us} us, {} us each",
        us / rounds.max(1)
    );
    // SAFETY: the child sees its pipe close and leaves.
    unsafe { libc::close(a[1]) };

    // The server goes: the open hangs up, the node goes.
    let _ = server.kill();
    let _ = server.wait();
    std::thread::sleep(Duration::from_millis(200));
    let ev = poll(fd, 1000);
    check(
        "with the server gone the open is hung up",
        ev & (libc::POLLHUP | libc::POLLERR) != 0,
    );
    check("a read is ENODEV", read(fd, &mut buf) == Err(libc::ENODEV));
    check("the node is gone", !std::path::Path::new(&path).exists());
    // SAFETY: fds of this process.
    unsafe {
        libc::close(fd);
        libc::close(waiter);
    }

    // The next server puts it back.
    let mut server = start_server(name);
    let fd = open(&path, libc::O_RDWR | libc::O_NONBLOCK);
    write(fd, b"otra");
    check(
        "the next server serves it again",
        read(fd, &mut buf) == Ok(4) && &buf[..4] == b"otra",
    );
    // SAFETY: an fd of this process.
    unsafe { libc::close(fd) };
    let _ = server.kill();
    let _ = server.wait();
    println!("{NAME}: dev-echo: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("serve"), Some(name)) => serve(name),
        (Some("check"), Some(name)) => {
            let rounds = args.get(3).and_then(|r| r.parse().ok()).unwrap_or(2000);
            run_check(name, rounds)
        }
        _ => die("usage: udrv-dev-echo serve NAME | check NAME [ROUNDS]"),
    }
}
