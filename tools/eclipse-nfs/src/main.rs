//! `eclipse-nfs [-f] [-r] [-o OPTIONS] HOST:/EXPORT DIR`: mount the NFSv3
//! export at `DIR` and serve it to the kernel until it is unmounted. `-r`
//! mounts it read-only; `-o` takes the options `mount -t nfs` takes
//! (`port=`, `mountport=`, `rsize=`, `wsize=`, `timeo=`, `retrans=`,
//! `addr=`, `ro`).
//!
//! The command returns once the export is mounted, as `mount` does, and
//! two processes stay behind: the server, and one that waits for it and
//! starts another when it dies. `-f` is the server alone, in the
//! foreground. `umount DIR` ends them.
//!
//! NFSv3 keeps nothing on the server that a client must hold on to, and
//! this keeps nothing the kernel does not: what is written goes to the NFS
//! server as it is written. So the next server after one that died greets
//! the NFS server again, and whoever was reading or writing goes on.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{IpAddr, ToSocketAddrs};
use std::os::fd::FromRawFd;
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};

use fatserve::wire::{
    Msg, DATA_AREA, NAME_AREA, OP_CREATE, OP_LIST, OP_READ, OP_REMOVE, OP_RENAME, OP_SETATTR,
    OP_SETTIME, OP_STAT, OP_STATFS, OP_SYNC, OP_TRUNCATE, OP_WRITE, SERVE_POSIX, SERVE_READ_ONLY,
    SERVE_TAKE_OVER, SLOTS, SLOT_LEN, STATFS_LEN,
};
use nfs3::connect::Connector;
use nfs3::greeting;
use nfs3::session::NfsSession;
use nfs3::source::{NfsOptions, NfsSource};
use nfsserve::export::Export;
use nfsserve::net::{Dial, Transport};
use udrivers::fifo::Fifo;
use udrivers::zx::{self, nr, Status};

const NAME: &str = "eclipse-nfs";

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("{NAME}: {why}");
    exit(1)
}

/// The window, and one request's parts of it.
struct Window {
    base: *mut u8,
}

impl Window {
    /// The path of the request in `slot`, `len` bytes of it.
    fn path(&self, slot: usize, len: usize) -> Option<&str> {
        if len > NAME_AREA {
            return None;
        }
        // SAFETY: inside the slot, which is inside the mapping. The kernel
        // wrote it before it sent the message and leaves the slot alone
        // until the reply.
        let bytes = unsafe { std::slice::from_raw_parts(self.base.add(slot * SLOT_LEN), len) };
        std::str::from_utf8(bytes).ok()
    }

    /// The data area of the request in `slot`.
    #[allow(clippy::mut_from_ref)]
    fn data(&self, slot: usize) -> &mut [u8] {
        // SAFETY: inside the mapping, and this process's to write until it
        // replies: one request is in flight on a slot, and this serves one
        // at a time.
        unsafe {
            std::slice::from_raw_parts_mut(self.base.add(slot * SLOT_LEN + NAME_AREA), DATA_AREA)
        }
    }
}

/// The NFS server stopped answering, or the connection to it is gone:
/// what this process ends with, so the next one greets it again.
const LOST: i32 = 4;

/// Answer `msg` in place.
fn answer(export: &mut Export<Transport>, window: &Window, msg: &mut Msg, read_only: bool) {
    let slot = msg.reqid as usize & (SLOTS - 1);
    let Some(path) = window.path(slot, msg.names as usize) else {
        msg.status = -libc::EINVAL as i64;
        return;
    };
    let data = window.data(slot);
    let record = |used: usize| (1, used);
    let done = match msg.op {
        OP_WRITE | OP_CREATE | OP_REMOVE | OP_RENAME | OP_TRUNCATE | OP_SETTIME | OP_SETATTR
            if read_only =>
        {
            Err(libc::EROFS)
        }
        OP_WRITE => {
            let len = (msg.b as usize).min(DATA_AREA);
            export.write(path, msg.a, &data[..len]).map(|n| (n, 0))
        }
        OP_CREATE => export.create(path, msg.a as u32, data).map(record),
        OP_REMOVE => export.remove(path).map(|()| (0, 0)),
        OP_RENAME => match path.split_once('\0') {
            Some((from, to)) => export.rename(from, to, data).map(record),
            None => Err(libc::EINVAL),
        },
        OP_TRUNCATE => export.truncate(path, msg.a).map(|()| (0, 0)),
        OP_SETTIME => export.set_time(path, msg.a as i64).map(|()| (0, 0)),
        OP_SETATTR => export
            .set_attr(path, (msg.a >> 32) as u32, msg.a as u32, msg.b)
            .map(|()| (0, 0)),
        OP_STAT => export.stat(path, data).map(record),
        OP_SYNC => export.commit().map(|()| (0, 0)),
        OP_LIST => export.list(path, msg.a, data).map(|(count, used, next)| {
            msg.a = next;
            (count, used)
        }),
        OP_READ => {
            let want = (msg.b as usize).min(DATA_AREA);
            export.read(path, msg.a, &mut data[..want]).map(|n| (n, n))
        }
        OP_STATFS => export.statfs(&mut data[..STATFS_LEN]).map(|n| (0, n)),
        _ => Err(libc::ENOSYS),
    };
    match done {
        Ok((count, used)) => {
            msg.status = count as i64;
            msg.b = used as u32;
        }
        // The connection is gone: the request is answered by the next
        // server, on a new one.
        Err(_) if export.session().transport().is_lost() => {
            eprintln!("{NAME}: the connection to the NFS server is gone");
            exit(LOST)
        }
        Err(errno) => msg.status = -errno as i64,
    }
}

/// The timer went off: what was written a second ago is due on the
/// server's disk.
static DUE: AtomicBool = AtomicBool::new(false);

extern "C" fn due(_signal: libc::c_int) {
    DUE.store(true, Ordering::Relaxed);
}

/// Have `SIGALRM` end the wait for requests instead of the process.
fn catch_the_timer() {
    // SAFETY: an all-zero `sigaction` is a valid one; the handler stores
    // a flag and nothing else. No `SA_RESTART`: the wait is to end.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = due as extern "C" fn(libc::c_int) as usize;
        libc::sigaction(libc::SIGALRM, &action, std::ptr::null_mut());
    }
}

/// How a server that was to take a mount over ends when there is none:
/// it was unmounted while it had no server.
const NOTHING_TO_TAKE: i32 = 3;

/// What the mount is of, as the command was given it.
struct Mount {
    source: String,
    parsed: NfsSource,
    opts: NfsOptions,
    addr: IpAddr,
    dir: String,
    read_only: bool,
}

/// The export, greeted and with its session open.
fn open_export(m: &Mount) -> Result<Export<Transport>, String> {
    let mut dial = Dial::new(m.addr);
    let budget = greeting::Budget::new(greeting::greeting_budget(&m.opts), dial.now_ms());
    let said = |e: nfs3::errno::Errno| std::io::Error::from_raw_os_error(e.0).to_string();
    let greeted =
        greeting::greet_with_deadline(&mut dial, &m.parsed, &m.opts, &budget).map_err(said)?;
    let left = budget.left(dial.now_ms()).map_err(said)?;
    let transport = dial.connect(greeted.nfs_port, left).map_err(said)?;
    let mut session = NfsSession::new(transport, dial.first_xid(), greeted.root, &m.opts);
    session.negotiate().map_err(said)?;
    Ok(Export::new(session, m.read_only))
}

/// Mount and serve, or with `take_over` only take over the mount that is
/// there. `mounted` is told when the mount is this server's.
fn serve(m: &Mount, take_over: bool, mounted: Option<File>) -> ! {
    let mut export = open_export(m).unwrap_or_else(|e| {
        if take_over {
            // The next one tries again: the NFS server may be back by then.
            eprintln!("{NAME}: {}: {e}", m.source);
            exit(LOST)
        }
        die(format_args!("{}: {e}", m.source))
    });
    let (fifo, kernels) =
        Fifo::<Msg>::create(SLOTS).unwrap_or_else(|e| die(format_args!("fifo_create: {e}")));
    let (base, kernels_window) = zx::shared_memory(SLOTS * SLOT_LEN).unwrap_or_else(|e| die(e));
    let window = Window { base };
    let names = format!("{}\0{}\0nfs", m.dir, m.source);
    let flags = SERVE_POSIX
        | if m.read_only { SERVE_READ_ONLY } else { 0 }
        | if take_over { SERVE_TAKE_OVER } else { 0 };
    // SAFETY: two handles, which go to the kernel, and `names`, which it
    // reads. Nothing is kept for the next server: there is nothing to keep.
    let given = unsafe {
        zx::call(
            nr::ECLIPSE_FS_SERVE,
            [
                kernels as usize,
                kernels_window as usize,
                names.as_ptr() as usize,
                names.len(),
                flags,
                0,
            ],
        )
    };
    match given {
        Ok(0) => {}
        Ok(_) => eprintln!("{NAME}: {}: took over from a server that is gone", m.dir),
        Err(Status::NOT_FOUND) if take_over => exit(NOTHING_TO_TAKE),
        Err(Status::NO_GATEWAY) => die("this kernel does not let a process serve a filesystem"),
        Err(Status(-30)) => die("mounting needs root"),
        Err(Status::NOT_FOUND) => die(format_args!("{}: no such directory", m.dir)),
        Err(Status::ALREADY_BOUND) => die(format_args!("{}: {} is served there", m.dir, m.source)),
        Err(e) => die(format_args!("eclipse_fs_serve: {e}")),
    }
    if let Some(mut mounted) = mounted {
        let _ = mounted.write_all(b"1");
    }

    catch_the_timer();
    let mut msgs = [Msg::default(); SLOTS];
    let mut replies = 0;
    let mut timed = false;
    loop {
        let n = match fifo.reply_wait_once(&mut msgs, replies) {
            Ok(n) => n,
            Err(Status::INTERNAL_INTR_RETRY) => 0,
            // Unmounted. The kernel asked for a sync first; this is for
            // one that did not.
            Err(Status::PEER_CLOSED) => {
                let _ = export.commit();
                exit(0)
            }
            Err(e) => die(format_args!("the kernel's requests: {e}")),
        };
        for msg in &mut msgs[..n] {
            answer(&mut export, &window, msg, m.read_only);
        }
        replies = n;
        if DUE.swap(false, Ordering::Relaxed) {
            timed = false;
            if let Err(e) = export.commit() {
                eprintln!("{NAME}: {}: commit failed (errno {e})", m.source);
            }
        }
        if !timed && export.is_dirty() {
            timed = true;
            // SAFETY: no pointers.
            unsafe { libc::alarm(1) };
        }
    }
}

/// The server's address: `addr=`, or the host resolved.
fn address(parsed: &NfsSource, opts: &NfsOptions) -> IpAddr {
    if let Some(addr) = opts.addr {
        return addr;
    }
    if let Ok(addr) = parsed.host.parse::<IpAddr>() {
        return addr;
    }
    (parsed.host.as_str(), 0)
        .to_socket_addrs()
        .ok()
        .and_then(|mut found| found.next())
        .map(|a| a.ip())
        .unwrap_or_else(|| die(format_args!("{}: no such host", parsed.host)))
}

fn usage() -> ! {
    eprintln!(
        "usage: {NAME} [-f] [-r] [-o OPTIONS] HOST:/EXPORT DIR\n\
         Mount the NFSv3 export at DIR, served by this process.\n\
         -f: stay in the foreground, with nothing to start the server again.\n\
         -r: read-only.\n\
         -o: the options of mount -t nfs (port=, mountport=, rsize=, wsize=, ...)."
    );
    exit(2)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut foreground, mut read_only, mut options) = (false, false, String::new());
    let mut rest = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-f" if rest.is_empty() => foreground = true,
            "-r" if rest.is_empty() => read_only = true,
            "-o" if rest.is_empty() => options = args.next().unwrap_or_else(|| usage()),
            _ => rest.push(arg),
        }
    }
    let [source, dir] = rest.as_slice() else {
        usage()
    };
    let parsed = NfsSource::parse(source)
        .unwrap_or_else(|_| die(format_args!("{source}: not HOST:/EXPORT")));
    let mut opts =
        NfsOptions::parse(&options).unwrap_or_else(|_| die(format_args!("-o {options}: bad")));
    read_only |= opts.read_only;
    opts.read_only = read_only;
    // A request of the kernel is up to a data area long: one READ or WRITE
    // of that much answers it, where the kernel's own default (32 KiB,
    // a bound on its heap) would take two calls. What the server offers
    // still caps it, and `rsize=` / `wsize=` still say otherwise.
    let given = |key: &str| options.split(',').any(|o| o.starts_with(key));
    if !given("rsize=") {
        opts.rsize = DATA_AREA;
    }
    if !given("wsize=") {
        opts.wsize = DATA_AREA;
    }
    let addr = address(&parsed, &opts);
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|e| die(format_args!("{dir}: {e}")));
    let dir = dir
        .to_str()
        .unwrap_or_else(|| die("DIR is not text"))
        .to_string();
    let m = Mount {
        source: source.clone(),
        parsed,
        opts,
        addr,
        dir,
        read_only,
    };
    if foreground {
        serve(&m, false, None);
    }
    let (mut told, tell) = pipe();
    // SAFETY: one thread, and the child goes on to `watch`.
    match unsafe { libc::fork() } {
        -1 => die(format_args!("fork: {}", std::io::Error::last_os_error())),
        0 => {
            drop(told);
            // SAFETY: no pointers.
            unsafe { libc::setsid() };
            watch(&m, tell)
        }
        _ => {
            drop(tell);
            let mut word = [0u8; 1];
            match told.read(&mut word) {
                Ok(1) => exit(0),
                _ => exit(1),
            }
        }
    }
}

/// A pipe: the end that reads, and the end that writes.
fn pipe() -> (File, File) {
    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: the call writes two descriptors into `ends`.
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        die(format_args!("pipe: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: the two descriptors are this function's own, each owned once.
    unsafe { (File::from_raw_fd(ends[0]), File::from_raw_fd(ends[1])) }
}

/// The servers that die this soon after they start, one after another,
/// that are tried before the mount is left without one.
const QUICK_DEATHS: u32 = 5;
/// A server that lived this long was a server.
const LIVED: std::time::Duration = std::time::Duration::from_secs(5);

/// Start the server of the mount, and another each time one dies, until
/// the mount is gone. `mounted` is told when the first has made the mount.
fn watch(m: &Mount, mounted: File) -> ! {
    let mut mounted = Some(mounted);
    let mut quick = 0;
    loop {
        let first = mounted.is_some();
        let started = std::time::Instant::now();
        let (mut told, tell) = pipe();
        // SAFETY: one thread, and the child goes on to `serve`.
        let server = match unsafe { libc::fork() } {
            -1 => die(format_args!("fork: {}", std::io::Error::last_os_error())),
            0 => {
                drop(told);
                drop(mounted);
                serve(m, !first, Some(tell))
            }
            server => server,
        };
        drop(tell);
        let mut word = [0u8; 1];
        let served = matches!(told.read(&mut word), Ok(1));
        drop(told);
        if served {
            if let Some(mut mounted) = mounted.take() {
                let _ = mounted.write_all(b"1");
            }
        }
        let mut status: libc::c_int = 0;
        // SAFETY: the call writes the child's status into `status`.
        while unsafe { libc::waitpid(server, &mut status, 0) } == -1
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {}
        let code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
        match code {
            Some(0) | Some(NOTHING_TO_TAKE) => exit(0),
            _ if first && !served => exit(1),
            _ => {}
        }
        quick = if served && started.elapsed() >= LIVED {
            0
        } else {
            quick + 1
        };
        if quick > QUICK_DEATHS {
            die(format_args!(
                "{}: its servers keep dying; it is left without one",
                m.dir
            ));
        }
        let why = match code {
            Some(code) => format!("ended with {code}"),
            None => format!("was killed by signal {}", libc::WTERMSIG(status)),
        };
        eprintln!("{NAME}: {}: its server {why}; starting another", m.dir);
        std::thread::sleep(std::time::Duration::from_millis(50 << quick.min(5)));
    }
}
