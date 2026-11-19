//! `eclipse-mountd`: the mount helper. The kernel hands it each `mount(2)`
//! of a filesystem a process serves -- FAT always, as the kernel has none;
//! btrfs, EclipseFS and NFS unless the boot says `ufs=kernel` -- and this
//! makes it with that filesystem's server and answers what `mount(2)`
//! returns: so `mount -t ...`, the installer and whatever else calls
//! `mount(2)` get a served mount without knowing.
//!
//! A service of init, started again when it dies; while there is none,
//! `mount(2)` of FAT is `ENODEV` and the others are the kernel's own. One
//! mount at a time: the next waits in the FIFO until the server of this one
//! has made it or said why not. What the kernel sends is in
//! `linux-object/src/fs/served/helper.rs`.

use std::process::{exit, Command};

use fatserve::wire::{Msg, DATA_AREA, NAME_AREA, SLOTS, SLOT_LEN};
use udrivers::fifo::Fifo;
use udrivers::zx::{self, nr, Status};

const NAME: &str = "eclipse-mountd";

/// There once this is the kernel's helper. `/run` is emptied at each boot.
const READY: &str = "/run/eclipse-mountd";

/// Make the mount: names `source\0target\0type`, `a` the flags of
/// `mount(2)`, `b` bytes of options in the data area.
const OP_MOUNT: u32 = 1;

/// `MS_RDONLY`, which the kernel sets for an `ro` among the options too.
const MS_RDONLY: u64 = 1;

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("{NAME}: {why}");
    exit(1)
}

/// The server of each type, and what a mount it does not make is.
struct Server {
    fstype: &'static str,
    program: &'static str,
    /// Its source is a path (a device or an image), not `HOST:/EXPORT`.
    on_a_file: bool,
    /// It is given the options (`-o`): NFS's are its own.
    takes_options: bool,
    /// The answer when it does not make the mount. `ENODEV` hands the mount
    /// to the kernel's own code, which mounts what the server does not take
    /// (a volume of several devices, an EclipseFS v0.3) and refuses the
    /// rest with its own errno. FAT has no such code: a volume its server
    /// does not read is not FAT, `EINVAL`. An NFS server that cannot be
    /// reached once is not tried twice: `EIO`, and the server said why.
    failed: i32,
}

const SERVERS: &[Server] = &[
    Server {
        fstype: "vfat",
        program: "/bin/eclipse-fatfs",
        on_a_file: true,
        takes_options: false,
        failed: libc::EINVAL,
    },
    Server {
        fstype: "btrfs",
        program: "/bin/eclipse-btrfs",
        on_a_file: true,
        takes_options: false,
        failed: libc::ENODEV,
    },
    Server {
        fstype: "eclipsefs",
        program: "/bin/eclipse-eclipsefs",
        on_a_file: true,
        takes_options: false,
        failed: libc::ENODEV,
    },
    Server {
        fstype: "nfs",
        program: "/bin/eclipse-nfs",
        on_a_file: false,
        takes_options: true,
        failed: libc::EIO,
    },
];

/// A mount the kernel asks for.
#[derive(Debug, PartialEq, Eq)]
struct Mount<'a> {
    source: &'a str,
    target: &'a str,
    fstype: &'a str,
    read_only: bool,
    options: &'a str,
}

impl<'a> Mount<'a> {
    /// The mount of `names` (`source\0target\0type`), the flags `a` and the
    /// option string `options`.
    fn parse(names: &'a [u8], a: u64, options: &'a [u8]) -> Result<Self, i32> {
        let names = std::str::from_utf8(names).map_err(|_| libc::EINVAL)?;
        let options = std::str::from_utf8(options).map_err(|_| libc::EINVAL)?;
        let mut parts = names.split('\0');
        let (Some(source), Some(target), Some(fstype), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(libc::EINVAL);
        };
        if source.is_empty() || !target.starts_with('/') {
            return Err(libc::EINVAL);
        }
        Ok(Mount {
            source,
            target,
            fstype,
            read_only: a & MS_RDONLY != 0,
            options,
        })
    }

    /// The server of its type.
    fn server(&self) -> Result<&'static Server, i32> {
        // A type the kernel did not ask for is one this has no server for,
        // as there is no module on Linux.
        SERVERS
            .iter()
            .find(|s| s.fstype == self.fstype)
            .ok_or(libc::ENODEV)
    }

    /// The command that makes it: its program and arguments.
    fn command(&self) -> Result<(&'static str, Vec<&'a str>), i32> {
        let server = self.server()?;
        if server.on_a_file && !self.source.starts_with('/') {
            return Err(libc::EINVAL);
        }
        let mut args = Vec::with_capacity(5);
        if self.read_only {
            args.push("-r");
        }
        if server.takes_options && !self.options.is_empty() {
            args.push("-o");
            args.push(self.options);
        }
        args.push(self.source);
        args.push(self.target);
        Ok((server.program, args))
    }
}

/// Make the mount asked for: 0, or a negative `errno`.
fn make(names: &[u8], a: u64, options: &[u8]) -> i64 {
    let mount = match Mount::parse(names, a, options) {
        Ok(mount) => mount,
        Err(errno) => return -errno as i64,
    };
    let (program, args) = match mount.command() {
        Ok(command) => command,
        Err(errno) => {
            if errno == libc::ENODEV {
                eprintln!("{NAME}: no server for {}", mount.fstype);
            }
            return -errno as i64;
        }
    };
    let failed = mount.server().map_or(libc::EIO, |s| s.failed);
    // The server returns once the mount is made, or has said why not on
    // its standard error, which is this service's.
    match Command::new(program).args(&args).status() {
        Ok(status) if status.success() => 0,
        Ok(_) => -failed as i64,
        // An image without the server: the kernel's own code where there
        // is one.
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
             Make the mounts mount(2) hands this, each with its server\n\
             (eclipse-fatfs, eclipse-btrfs, eclipse-eclipsefs, eclipse-nfs).\n\
             Runs until killed."
        );
        exit(2)
    }
    let (fifo, base) = register();
    // Said where whoever mounts at boot waits for it (`eclipse-fs-mounts`):
    // a mount(2) before this is the kernel's own.
    let _ = std::fs::write(READY, b"");
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
                    let (names, options) = unsafe {
                        let at = base.add(slot * SLOT_LEN);
                        (
                            std::slice::from_raw_parts(at, msg.names as usize),
                            std::slice::from_raw_parts(at.add(NAME_AREA), msg.b as usize),
                        )
                    };
                    make(names, msg.a, options)
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
    fn a_mount_is_three_names_the_flags_and_the_options() {
        let m = Mount::parse(b"/dev/sda1\0/boot/efi\0vfat", 0, b"").unwrap();
        assert_eq!(
            m,
            Mount {
                source: "/dev/sda1",
                target: "/boot/efi",
                fstype: "vfat",
                read_only: false,
                options: "",
            }
        );
        let m = Mount::parse(b"/a\0/b\0btrfs", MS_RDONLY | 0x400, b"noatime").unwrap();
        assert!(m.read_only);
        assert_eq!(m.options, "noatime");
        for bad in [
            &b"/a\0/b"[..],
            b"/a\0/b\0vfat\0x",
            b"\0/b\0vfat",
            b"/a\0b\0vfat",
            b"/a\0/b\0\xff",
        ] {
            assert_eq!(Mount::parse(bad, 0, b""), Err(libc::EINVAL), "{bad:?}");
        }
        assert_eq!(Mount::parse(b"/a\0/b\0nfs", 0, b"\xff"), Err(libc::EINVAL));
    }

    #[test]
    fn each_type_is_made_by_its_server() {
        let command = |names: &[u8], a, options: &[u8]| {
            Mount::parse(names, a, options).and_then(|m| {
                m.command()
                    .map(|(p, args)| (p, args.iter().map(|a| a.to_string()).collect::<Vec<_>>()))
            })
        };
        let words = |w: &[&str]| w.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert_eq!(
            command(b"/img\0/mnt\0vfat", 0, b"umask=077"),
            Ok(("/bin/eclipse-fatfs", words(&["/img", "/mnt"])))
        );
        assert_eq!(
            command(b"/dev/sdb1\0/mnt\0btrfs", MS_RDONLY, b""),
            Ok(("/bin/eclipse-btrfs", words(&["-r", "/dev/sdb1", "/mnt"])))
        );
        assert_eq!(
            command(b"/dev/vda2\0/home\0eclipsefs", 0, b"noatime"),
            Ok(("/bin/eclipse-eclipsefs", words(&["/dev/vda2", "/home"])))
        );
        assert_eq!(
            command(b"srv:/export\0/mnt\0nfs", 0, b"vers=3,addr=10.0.2.2"),
            Ok((
                "/bin/eclipse-nfs",
                words(&["-o", "vers=3,addr=10.0.2.2", "srv:/export", "/mnt"])
            ))
        );
        assert_eq!(
            command(b"srv:/export\0/mnt\0nfs", MS_RDONLY, b""),
            Ok(("/bin/eclipse-nfs", words(&["-r", "srv:/export", "/mnt"])))
        );
        // A block filesystem's source is a path; NFS's is not.
        assert_eq!(command(b"srv:/x\0/mnt\0btrfs", 0, b""), Err(libc::EINVAL));
        assert_eq!(command(b"/img\0/mnt\0ext4", 0, b""), Err(libc::ENODEV));
        assert_eq!(make(b"/img\0/mnt\0ext4", 0, b""), -libc::ENODEV as i64);
    }

    /// What a mount its server did not make is: the kernel's own for btrfs
    /// and EclipseFS, which have kernel code to fall back to.
    #[test]
    fn a_mount_a_server_does_not_make_goes_back_to_the_kernel_where_it_can() {
        let failed = |t: &str| SERVERS.iter().find(|s| s.fstype == t).unwrap().failed;
        assert_eq!(failed("btrfs"), libc::ENODEV);
        assert_eq!(failed("eclipsefs"), libc::ENODEV);
        assert_eq!(failed("vfat"), libc::EINVAL);
        assert_eq!(failed("nfs"), libc::EIO);
    }
}
