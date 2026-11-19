//! `eclipse-mountd`: the mount helper. The kernel has no FAT, and hands
//! each `mount(2)` of `vfat` to this process, which makes it with a server
//! (`eclipse-fatfs`) and answers what `mount(2)` returns: so `mount -t
//! vfat`, the installer and whatever else calls `mount(2)` mount FAT as
//! they did when the kernel had its own.
//!
//! A service of init, started again when it dies; while there is none,
//! `mount(2)` of FAT is `ENODEV`. One mount at a time: the next waits in
//! the FIFO until the server of this one has made it or said why not.
//! What the kernel sends is in `linux-object/src/fs/served/helper.rs`.

use std::process::{exit, Command};

use fatserve::wire::{Msg, DATA_AREA, NAME_AREA, SLOTS, SLOT_LEN};
use udrivers::fifo::Fifo;
use udrivers::zx::{self, nr, Status};

const NAME: &str = "eclipse-mountd";

/// Make the mount: names `source\0target\0type`, `a` the flags of
/// `mount(2)`, `b` bytes of options in the data area.
const OP_MOUNT: u32 = 1;

/// `MS_RDONLY`, which the kernel sets for an `ro` among the options too.
const MS_RDONLY: u64 = 1;

/// The server of a FAT volume.
const FATFS: &str = "/bin/eclipse-fatfs";

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("{NAME}: {why}");
    exit(1)
}

/// A mount the kernel asks for.
#[derive(Debug, PartialEq, Eq)]
struct Mount<'a> {
    source: &'a str,
    target: &'a str,
    fstype: &'a str,
    read_only: bool,
}

impl<'a> Mount<'a> {
    /// The mount of `names` (`source\0target\0type`) and the flags `a`.
    fn parse(names: &'a [u8], a: u64) -> Result<Self, i32> {
        let names = std::str::from_utf8(names).map_err(|_| libc::EINVAL)?;
        let mut parts = names.split('\0');
        let (Some(source), Some(target), Some(fstype), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(libc::EINVAL);
        };
        if !source.starts_with('/') || !target.starts_with('/') {
            return Err(libc::EINVAL);
        }
        Ok(Mount {
            source,
            target,
            fstype,
            read_only: a & MS_RDONLY != 0,
        })
    }

    /// The command that makes it: its program and arguments.
    fn command(&self) -> Result<(&'static str, Vec<&'a str>), i32> {
        // The kernel asks for no other type; one it did would be a type
        // this has no server for, as there is no module on Linux.
        if self.fstype != "vfat" {
            return Err(libc::ENODEV);
        }
        let mut args = Vec::with_capacity(3);
        if self.read_only {
            args.push("-r");
        }
        args.push(self.source);
        args.push(self.target);
        Ok((FATFS, args))
    }
}

/// Make the mount `msg` asks for: 0, or a negative `errno`.
fn make(names: &[u8], a: u64) -> i64 {
    let mount = match Mount::parse(names, a) {
        Ok(mount) => mount,
        Err(errno) => return -errno as i64,
    };
    let (program, args) = match mount.command() {
        Ok(command) => command,
        Err(errno) => {
            eprintln!("{NAME}: no server for {}", mount.fstype);
            return -errno as i64;
        }
    };
    // The server returns once the mount is made, or has said why not on
    // its standard error, which is this service's.
    match Command::new(program).args(&args).status() {
        Ok(status) if status.success() => 0,
        // A volume the server cannot read, a place it cannot mount on:
        // `mount(2)` says EINVAL for a volume that is not of its type.
        Ok(_) => -libc::EINVAL as i64,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("{NAME}: {program} is not in this image");
            -libc::ENODEV as i64
        }
        Err(e) => {
            eprintln!("{NAME}: {program}: {e}");
            -libc::EIO as i64
        }
    }
}

/// The tries [`register`] makes while the helper before this one is still
/// there, a tenth of a second apart: the one that was killed is gone for
/// the kernel once its process is.
const TRIES: u32 = 20;

/// Be the kernel's mount helper: the FIFO it asks on and the window.
fn register() -> (Fifo<Msg>, *mut u8) {
    let mut tries = 0;
    loop {
        let (fifo, kernels) =
            Fifo::<Msg>::create(SLOTS).unwrap_or_else(|e| die(format_args!("fifo_create: {e}")));
        // A window of a try that is refused stays mapped, and is never
        // touched: a megabyte of addresses, not of memory.
        let (base, kernels_window) = zx::shared_memory(SLOTS * SLOT_LEN).unwrap_or_else(|e| die(e));
        // SAFETY: two handles, which go to the kernel.
        let made = unsafe {
            zx::call(
                nr::ECLIPSE_MOUNT_HELP,
                [kernels as usize, kernels_window as usize, 0, 0, 0, 0],
            )
        };
        match made {
            Ok(_) => return (fifo, base),
            Err(Status::ALREADY_BOUND) if tries < TRIES => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(Status::NO_GATEWAY) => die("this kernel does not ask a process for mounts"),
            Err(Status::ACCESS_DENIED) => die("mounting needs root"),
            Err(Status::ALREADY_BOUND) => die("there is a mount helper already"),
            Err(e) => die(format_args!("eclipse_mount_help: {e}")),
        }
    }
}

fn main() {
    if std::env::args().len() > 1 {
        eprintln!(
            "usage: {NAME}\n\
             Make the FAT mounts mount(2) is asked for, each with a server\n\
             (eclipse-fatfs). Runs until killed."
        );
        exit(2)
    }
    let (fifo, base) = register();
    let mut msgs = [Msg::default(); SLOTS];
    let mut replies = 0;
    loop {
        let n = match fifo.reply_wait_once(&mut msgs, replies) {
            Ok(n) => n,
            Err(Status::INTERNAL_INTR_RETRY) => 0,
            Err(Status::PEER_CLOSED) => exit(0),
            Err(e) => die(format_args!("the kernel's requests: {e}")),
        };
        for msg in &mut msgs[..n] {
            let slot = msg.reqid as usize & (SLOTS - 1);
            msg.status = match msg.op {
                OP_MOUNT if msg.names as usize <= NAME_AREA && msg.b as usize <= DATA_AREA => {
                    // SAFETY: inside the slot, which is inside the mapping;
                    // the kernel wrote it before the message and leaves it
                    // alone until the reply.
                    let names = unsafe {
                        std::slice::from_raw_parts(base.add(slot * SLOT_LEN), msg.names as usize)
                    };
                    make(names, msg.a)
                }
                OP_MOUNT => -libc::EINVAL as i64,
                _ => -libc::ENOSYS as i64,
            };
            msg.b = 0;
        }
        replies = n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mount_is_three_names_and_the_flags() {
        let m = Mount::parse(b"/dev/sda1\0/boot/efi\0vfat", 0).unwrap();
        assert_eq!(
            m,
            Mount {
                source: "/dev/sda1",
                target: "/boot/efi",
                fstype: "vfat",
                read_only: false
            }
        );
        assert!(
            Mount::parse(b"/a\0/b\0vfat", MS_RDONLY | 0x400)
                .unwrap()
                .read_only
        );
        for bad in [
            &b"/a\0/b"[..],
            b"/a\0/b\0vfat\0x",
            b"a\0/b\0vfat",
            b"/a\0b\0vfat",
            b"/a\0/b\0\xff",
        ] {
            assert_eq!(Mount::parse(bad, 0), Err(libc::EINVAL), "{bad:?}");
        }
    }

    #[test]
    fn fat_is_made_by_its_server_and_nothing_else_is() {
        let m = Mount::parse(b"/img\0/mnt\0vfat", 0).unwrap();
        assert_eq!(m.command(), Ok((FATFS, vec!["/img", "/mnt"])));
        let m = Mount::parse(b"/img\0/mnt\0vfat", MS_RDONLY).unwrap();
        assert_eq!(m.command(), Ok((FATFS, vec!["-r", "/img", "/mnt"])));
        let m = Mount::parse(b"/img\0/mnt\0ext4", 0).unwrap();
        assert_eq!(m.command(), Err(libc::ENODEV));
        assert_eq!(make(b"/img\0/mnt\0ext4", 0), -libc::ENODEV as i64);
    }
}
