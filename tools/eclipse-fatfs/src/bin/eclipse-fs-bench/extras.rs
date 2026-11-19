//! `eclipse-fs-bench -x DIR`: whether the filesystem under `DIR` does
//! what EclipseFS does past reading and writing, each as a program asks
//! for it: a reflink (`FICLONE`, and `copy_file_range`), extended
//! attributes, a file's flags (`chattr`), `fallocate` with a punched hole,
//! `lseek` to data and to a hole, `FS_IOC_FIEMAP`, and a subvolume made
//! and deleted by ioctl. One line for each, `ok` or what went wrong, in a
//! directory it makes under `DIR` and removes; it ends with the count, and
//! fails when one did.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;

const FICLONE: libc::c_ulong = 0x4004_9409;
const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;
const FS_IOC_FIEMAP: libc::c_ulong = 0xc020_660b;
const BTRFS_IOC_SUBVOL_CREATE: libc::c_ulong = 0x5000_940e;
const BTRFS_IOC_SNAP_DESTROY: libc::c_ulong = 0x5000_940f;
const FS_NOCOW_FL: libc::c_int = 0x0080_0000;

/// One check: `Err` says what went wrong.
type Check = fn(&Path) -> Result<(), String>;

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed))
        .collect()
}

fn read_all(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
}

fn reflink(dir: &Path) -> Result<(), String> {
    let data = pattern(256 * 1024, 1);
    let src = dir.join("src");
    fs::write(&src, &data).map_err(|e| e.to_string())?;
    let s = File::open(&src).map_err(|e| e.to_string())?;
    let d = File::create(dir.join("clone")).map_err(|e| e.to_string())?;
    // SAFETY: the argument is the source's descriptor.
    if unsafe { libc::ioctl(d.as_raw_fd(), FICLONE as _, s.as_raw_fd()) } != 0 {
        return Err(format!("FICLONE: {}", errno()));
    }
    drop(d);
    if read_all(&dir.join("clone"))? != data {
        return Err("the clone reads otherwise".into());
    }
    Ok(())
}

fn copy_range(dir: &Path) -> Result<(), String> {
    let data = read_all(&dir.join("src"))?;
    let s = File::open(dir.join("src")).map_err(|e| e.to_string())?;
    let d = File::create(dir.join("copy")).map_err(|e| e.to_string())?;
    let mut done = 0usize;
    while done < data.len() {
        // SAFETY: null offsets: each file's own position moves.
        let n = unsafe {
            libc::copy_file_range(
                s.as_raw_fd(),
                std::ptr::null_mut(),
                d.as_raw_fd(),
                std::ptr::null_mut(),
                data.len() - done,
                0,
            )
        };
        if n <= 0 {
            return Err(format!("copy_file_range: {} after {done}", errno()));
        }
        done += n as usize;
    }
    drop(d);
    if read_all(&dir.join("copy"))? != data {
        return Err("the copy reads otherwise".into());
    }
    Ok(())
}

fn attributes(dir: &Path) -> Result<(), String> {
    let p = cpath(&dir.join("src"));
    let name = c"user.color";
    // SAFETY: NUL-terminated strings and a buffer of the length given.
    let r = unsafe { libc::setxattr(p.as_ptr(), name.as_ptr(), b"blue".as_ptr().cast(), 4, 0) };
    if r != 0 {
        return Err(format!("setxattr: {}", errno()));
    }
    let mut value = [0u8; 16];
    // SAFETY: as above.
    let n = unsafe { libc::getxattr(p.as_ptr(), name.as_ptr(), value.as_mut_ptr().cast(), 16) };
    if n != 4 || &value[..4] != b"blue" {
        return Err(format!("getxattr: {n} {}", errno()));
    }
    let mut list = [0u8; 64];
    // SAFETY: as above.
    let n = unsafe { libc::listxattr(p.as_ptr(), list.as_mut_ptr().cast(), 64) };
    if n < 0 || !list[..n as usize].starts_with(b"user.color\0") {
        return Err(format!("listxattr: {n} {}", errno()));
    }
    // SAFETY: as above.
    if unsafe { libc::removexattr(p.as_ptr(), name.as_ptr()) } != 0 {
        return Err(format!("removexattr: {}", errno()));
    }
    // SAFETY: as above.
    let n = unsafe { libc::getxattr(p.as_ptr(), name.as_ptr(), value.as_mut_ptr().cast(), 16) };
    if n >= 0 || errno().raw_os_error() != Some(libc::ENODATA) {
        return Err(format!("removed, getxattr: {n} {}", errno()));
    }
    Ok(())
}

fn flags(dir: &Path) -> Result<(), String> {
    let f = File::create(dir.join("flagged")).map_err(|e| e.to_string())?;
    let mut flags: libc::c_int = 0;
    // SAFETY: the argument is an int to read into, then one to set.
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS as _, &mut flags) } != 0 {
        return Err(format!("FS_IOC_GETFLAGS: {}", errno()));
    }
    let mut set = flags | FS_NOCOW_FL;
    // SAFETY: as above.
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_SETFLAGS as _, &mut set) } != 0 {
        return Err(format!("FS_IOC_SETFLAGS: {}", errno()));
    }
    // SAFETY: as above.
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS as _, &mut flags) } != 0
        || flags & FS_NOCOW_FL == 0
    {
        return Err(format!("the flag is not kept: {flags:#x}"));
    }
    Ok(())
}

fn holes(dir: &Path) -> Result<(), String> {
    let path = dir.join("holes");
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    f.write_all(&pattern(64 * 1024, 2))
        .map_err(|e| e.to_string())?;
    let fd = f.as_raw_fd();
    // Grown by preallocation, then the first 16 KiB punched out.
    // SAFETY: plain calls on an open descriptor.
    if unsafe { libc::fallocate(fd, 0, 64 * 1024, 64 * 1024) } != 0 {
        return Err(format!("fallocate: {}", errno()));
    }
    if fs::metadata(&path).map_err(|e| e.to_string())?.len() != 128 * 1024 {
        return Err("fallocate did not grow the file".into());
    }
    let punch = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    // SAFETY: as above.
    if unsafe { libc::fallocate(fd, punch, 0, 16 * 1024) } != 0 {
        return Err(format!("punch: {}", errno()));
    }
    let mut first = [0xffu8; 4096];
    f.read_exact_at(&mut first, 0).map_err(|e| e.to_string())?;
    if first != [0; 4096] {
        return Err("the hole reads otherwise".into());
    }
    // SAFETY: as above.
    let data = unsafe { libc::lseek(fd, 0, libc::SEEK_DATA) };
    // SAFETY: as above.
    let hole = unsafe { libc::lseek(fd, 0, libc::SEEK_HOLE) };
    if data != 16 * 1024 || hole != 0 {
        return Err(format!("SEEK_DATA {data}, SEEK_HOLE {hole}: {}", errno()));
    }
    Ok(())
}

fn fiemap(dir: &Path) -> Result<(), String> {
    let f = File::open(dir.join("src")).map_err(|e| e.to_string())?;
    // `struct fiemap` with room for 32 extents, FIEMAP_FLAG_SYNC.
    let mut buf = vec![0u64; (32 + 56 * 32) / 8];
    buf[1] = u64::MAX;
    buf[2] = 1;
    buf[3] = 32 << 32;
    // SAFETY: the buffer holds the header and the extents it says.
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FIEMAP as _, buf.as_mut_ptr()) } != 0 {
        return Err(format!("FS_IOC_FIEMAP: {}", errno()));
    }
    let mapped = buf[2] >> 32;
    if mapped == 0 {
        return Err("no extent mapped".into());
    }
    Ok(())
}

fn subvolume(dir: &Path) -> Result<(), String> {
    let d = File::open(dir).map_err(|e| e.to_string())?;
    // `struct btrfs_ioctl_vol_args`: a descriptor, then the name.
    let mut args = vec![0u8; 4096];
    args[8..11].copy_from_slice(b"sub");
    // SAFETY: the argument is the struct, 4096 bytes.
    if unsafe {
        libc::ioctl(
            d.as_raw_fd(),
            BTRFS_IOC_SUBVOL_CREATE as _,
            args.as_mut_ptr(),
        )
    } != 0
    {
        return Err(format!("BTRFS_IOC_SUBVOL_CREATE: {}", errno()));
    }
    if !dir.join("sub").is_dir() {
        return Err("the subvolume is not listed".into());
    }
    fs::write(dir.join("sub/inside"), b"x").map_err(|e| e.to_string())?;
    fs::remove_file(dir.join("sub/inside")).map_err(|e| e.to_string())?;
    // SAFETY: as above.
    if unsafe {
        libc::ioctl(
            d.as_raw_fd(),
            BTRFS_IOC_SNAP_DESTROY as _,
            args.as_mut_ptr(),
        )
    } != 0
    {
        return Err(format!("BTRFS_IOC_SNAP_DESTROY: {}", errno()));
    }
    if dir.join("sub").exists() {
        return Err("the deleted subvolume is still listed".into());
    }
    Ok(())
}

pub fn extras(top: &Path) {
    let dir = top.join("fs-extras");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap_or_else(|e| super::die(format_args!("{}: {e}", dir.display())));
    let checks: [(&str, Check); 7] = [
        ("ficlone", reflink),
        ("copy_file_range", copy_range),
        ("xattr", attributes),
        ("flags", flags),
        ("fallocate+seek", holes),
        ("fiemap", fiemap),
        ("subvolume", subvolume),
    ];
    let mut failed = 0;
    for (name, check) in checks {
        match check(&dir) {
            Ok(()) => println!("{name:<16} ok"),
            Err(why) => {
                failed += 1;
                println!("{name:<16} FAIL {why}");
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
    println!("extras: {} ok, {failed} failed", checks.len() - failed);
    if failed > 0 {
        std::process::exit(1);
    }
}
