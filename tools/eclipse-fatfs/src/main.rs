//! `eclipse-fatfs [-f] DEVICE DIR`: mount the FAT volume on `DEVICE` at
//! `DIR`, read-only, and serve it to the kernel until it is unmounted.
//!
//! The command returns once the volume is mounted, as `mount` does, and
//! the server stays behind; `-f` keeps it in the foreground, for whoever
//! watches it and starts it again. `umount DIR` ends it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::exit;

use fatserve::volume::Volume;
use fatserve::wire::{
    Msg, DATA_AREA, NAME_AREA, OP_LIST, OP_READ, OP_STATFS, SERVE_FOLD_CASE, SLOTS, SLOT_LEN,
    STATFS_LEN,
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

/// Answer `msg` in place.
fn answer(volume: &mut Volume<File>, window: &Window, msg: &mut Msg) {
    let slot = msg.reqid as usize & (SLOTS - 1);
    let Some(path) = window.path(slot, msg.names as usize) else {
        msg.status = -libc::EINVAL as i64;
        return;
    };
    let data = window.data(slot);
    let done = match msg.op {
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

/// Mount and serve. `mounted` is told when the mount is made.
fn serve(device: &str, dir: &str, mounted: Option<File>) -> ! {
    let mut file = File::open(device).unwrap_or_else(|e| die(format_args!("{device}: {e}")));
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
                SERVE_FOLD_CASE,
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

    let mut msgs = [Msg::default(); SLOTS];
    let mut replies = 0;
    loop {
        let n = match fifo.reply_wait(&mut msgs, replies) {
            Ok(n) => n,
            // Unmounted.
            Err(Status::PEER_CLOSED) => exit(0),
            Err(e) => die(format_args!("the kernel's requests: {e}")),
        };
        for msg in &mut msgs[..n] {
            answer(&mut volume, &window, msg);
        }
        replies = n;
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let foreground = args.first().is_some_and(|a| a == "-f");
    if foreground {
        args.remove(0);
    }
    let [device, dir] = args.as_slice() else {
        eprintln!(
            "usage: {NAME} [-f] DEVICE DIR\n\
             Mount the FAT volume on DEVICE at DIR, read-only, served by this\n\
             process. -f: stay in the foreground."
        );
        exit(2)
    };
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|e| die(format_args!("{dir}: {e}")));
    let dir = dir.to_str().unwrap_or_else(|| die("DIR is not text"));
    if foreground {
        serve(device, dir, None);
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
            serve(device, dir, Some(tell))
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
