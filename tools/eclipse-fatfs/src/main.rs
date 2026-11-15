//! `eclipse-fatfs [-f] [-r] DEVICE DIR`: mount the FAT volume on `DEVICE`
//! at `DIR` and serve it to the kernel until it is unmounted. `-r` mounts
//! it read-only.
//!
//! The command returns once the volume is mounted, as `mount` does, and
//! the server stays behind; `-f` keeps it in the foreground, for whoever
//! watches it and starts it again. `umount DIR` ends it.
//!
//! What is written is in this process until it reaches the device: when
//! the kernel asks (`fsync`, `sync`, the unmount), and a second after the
//! first write that has not, so that a server that is killed takes at most
//! that second of writes with it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};

use fatserve::volume::Volume;
use fatserve::wire::{
    Msg, CREATE_DIR, DATA_AREA, NAME_AREA, OP_CREATE, OP_LIST, OP_READ, OP_REMOVE, OP_RENAME,
    OP_SETTIME, OP_STATFS, OP_SYNC, OP_TRUNCATE, OP_WRITE, SERVE_FOLD_CASE, SERVE_READ_ONLY, SLOTS,
    SLOT_LEN, STATFS_LEN,
};
use udrivers::fifo::Fifo;
use udrivers::zx::{self, nr, Status};

const NAME: &str = "eclipse-fatfs";

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("{NAME}: {why}");
    exit(1)
}

/// The bytes the device holds: where its end is, or for a device node that
/// has no end to seek to, what it says its size is.
fn device_len(device: &mut File) -> u64 {
    /// `BLKGETSIZE64`.
    const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
    let end = device.seek(SeekFrom::End(0)).unwrap_or(0);
    if end > 0 {
        return end;
    }
    let mut len: u64 = 0;
    // SAFETY: the call writes a u64 into `len`.
    let r = unsafe { libc::ioctl(device.as_raw_fd(), BLKGETSIZE64 as _, &mut len as *mut u64) };
    if r == 0 {
        len
    } else {
        0
    }
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

/// Answer `msg` in place. Nothing is written when `read_only`, whatever
/// is asked.
fn answer(volume: &mut Volume<File>, window: &Window, msg: &mut Msg, read_only: bool) {
    let slot = msg.reqid as usize & (SLOTS - 1);
    let Some(path) = window.path(slot, msg.names as usize) else {
        msg.status = -libc::EINVAL as i64;
        return;
    };
    let data = window.data(slot);
    // One entry, as the record a listing has for it.
    let record = |entry: fatserve::wire::Entry, data: &mut [u8]| {
        let (count, used, _) = fatserve::wire::pack(&[entry], 0, data);
        (count, used)
    };
    let done = match msg.op {
        OP_WRITE | OP_CREATE | OP_REMOVE | OP_RENAME | OP_TRUNCATE | OP_SETTIME if read_only => {
            Err(libc::EROFS)
        }
        OP_WRITE => {
            let len = (msg.b as usize).min(DATA_AREA);
            volume.write(path, msg.a, &data[..len]).map(|n| (n, 0))
        }
        OP_CREATE => volume
            .create(path, msg.a == CREATE_DIR)
            .map(|entry| record(entry, data)),
        OP_REMOVE => volume.remove(path).map(|()| (0, 0)),
        OP_RENAME => match path.split_once('\0') {
            Some((from, to)) => volume.rename(from, to).map(|entry| record(entry, data)),
            None => Err(libc::EINVAL),
        },
        OP_TRUNCATE => volume.truncate(path, msg.a).map(|()| (0, 0)),
        OP_SETTIME => volume.set_time(path, msg.a as i64).map(|()| (0, 0)),
        OP_SYNC => volume.sync().map(|()| (0, 0)),
        OP_LIST => volume.list(path, msg.a, data).map(|(count, used, next)| {
            msg.a = next;
            (count, used)
        }),
        OP_READ => {
            let want = (msg.b as usize).min(DATA_AREA);
            volume.read(path, msg.a, &mut data[..want]).map(|n| (n, n))
        }
        OP_STATFS => volume.statfs(&mut data[..STATFS_LEN]).map(|n| (0, n)),
        _ => Err(libc::ENOSYS),
    };
    match done {
        Ok((count, used)) => {
            msg.status = count as i64;
            msg.b = used as u32;
        }
        Err(errno) => msg.status = -errno as i64,
    }
}

/// The timer went off: what was written a second ago is due on the device.
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

/// Mount and serve. `mounted` is told when the mount is made.
fn serve(device: &str, dir: &str, read_only: bool, mounted: Option<File>) -> ! {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(!read_only)
        .open(device)
        .unwrap_or_else(|e| die(format_args!("{device}: {e}")));
    let len = device_len(&mut file);
    if len == 0 {
        die(format_args!("{device}: no size"));
    }
    let mut volume = Volume::open(file, len)
        .unwrap_or_else(|e| die(format_args!("{device}: not a FAT volume ({e})")));

    let (fifo, kernels) =
        Fifo::<Msg>::create(SLOTS).unwrap_or_else(|e| die(format_args!("fifo_create: {e}")));
    let (base, kernels_window) = zx::shared_memory(SLOTS * SLOT_LEN).unwrap_or_else(|e| die(e));
    let window = Window { base };
    let names = format!("{dir}\0{device}\0vfat");
    // SAFETY: two handles, which go to the kernel, and `names`, which it
    // reads.
    if let Err(e) = unsafe {
        zx::call(
            nr::ECLIPSE_FS_SERVE,
            [
                kernels as usize,
                kernels_window as usize,
                names.as_ptr() as usize,
                names.len(),
                SERVE_FOLD_CASE | if read_only { SERVE_READ_ONLY } else { 0 },
                0,
            ],
        )
    } {
        match e {
            Status::NO_GATEWAY => die("this kernel does not let a process serve a filesystem"),
            Status(-30) => die("mounting needs root"),
            Status::NOT_FOUND => die(format_args!("{dir}: no such directory")),
            e => die(format_args!("eclipse_fs_serve: {e}")),
        }
    }
    if let Some(mut mounted) = mounted {
        let _ = mounted.write_all(b"1");
    }

    catch_the_timer();
    let mut msgs = [Msg::default(); SLOTS];
    let mut replies = 0;
    // The timer is set, for what was written and has not reached the
    // device.
    let mut timed = false;
    loop {
        let n = match fifo.reply_wait_once(&mut msgs, replies) {
            Ok(n) => n,
            // A signal: the timer's, looked at below.
            Err(Status::INTERNAL_INTR_RETRY) => 0,
            // Unmounted. The kernel asked for a sync first; this is for
            // one that did not.
            Err(Status::PEER_CLOSED) => {
                if !read_only {
                    let _ = volume.close();
                }
                exit(0)
            }
            Err(e) => die(format_args!("the kernel's requests: {e}")),
        };
        for msg in &mut msgs[..n] {
            answer(&mut volume, &window, msg, read_only);
        }
        replies = n;
        if DUE.swap(false, Ordering::Relaxed) {
            timed = false;
            if let Err(e) = volume.flush() {
                eprintln!("{NAME}: {device}: writing failed (errno {e})");
            }
        }
        if !timed && volume.is_dirty() {
            timed = true;
            // SAFETY: no pointers.
            unsafe { libc::alarm(1) };
        }
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut flag = |flag: &str| {
        let at = args
            .iter()
            .take_while(|a| a.starts_with('-'))
            .position(|a| a == flag);
        at.map(|at| args.remove(at)).is_some()
    };
    let (foreground, read_only) = (flag("-f"), flag("-r"));
    let [device, dir] = args.as_slice() else {
        eprintln!(
            "usage: {NAME} [-f] [-r] DEVICE DIR\n\
             Mount the FAT volume on DEVICE at DIR, served by this process.\n\
             -f: stay in the foreground. -r: read-only."
        );
        exit(2)
    };
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|e| die(format_args!("{dir}: {e}")));
    let dir = dir.to_str().unwrap_or_else(|| die("DIR is not text"));
    if foreground {
        serve(device, dir, read_only, None);
    }

    // The server is a child, and this waits for its word that the mount
    // is made: the pipe closing without it is the mount failing.
    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: the call writes two descriptors into `ends`.
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        die(format_args!("pipe: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: the two descriptors are this function's own, each owned once.
    let (mut told, tell) = unsafe { (File::from_raw_fd(ends[0]), File::from_raw_fd(ends[1])) };
    // SAFETY: one thread, and the child goes on to `serve`.
    match unsafe { libc::fork() } {
        -1 => die(format_args!("fork: {}", std::io::Error::last_os_error())),
        0 => {
            drop(told);
            // A session of its own: the shell that ran the command is not
            // what ends the mount.
            // SAFETY: no pointers.
            unsafe { libc::setsid() };
            serve(device, dir, read_only, Some(tell))
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
