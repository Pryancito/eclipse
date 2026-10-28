//! `st_dev`: every filesystem is a device of its own, as every superblock is
//! on Linux (measured there with glibc and musl): `/`, `/proc`, `/sys`,
//! `/dev`, `/dev/shm`, `/tmp` and `/run` are seven devices, everything in each
//! is on it, and pipes, sockets and the anonymous objects are on pipefs,
//! sockfs and anon_inodefs. All of them said 0 but `/sys`, so to `find`,
//! `du -x`, `mountpoint` and `df` they were one device, and `/`, `/dev` and
//! `/dev/shm`, all inode 1, were one directory.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use linux_object::net::UnixSocketState;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

fn c(s: &str) -> String {
    format!("{}\0", s)
}

/// `(st_dev, st_ino)` of what `fill` writes.
fn dev_ino(fill: impl FnOnce(UserOutPtr<Stat>) -> SysResult) -> LxResult<(u64, u64)> {
    // Room for a `Stat`, aligned for it; its fields are read back by offset.
    let mut st = [0u64; 32];
    assert!(core::mem::size_of::<Stat>() <= core::mem::size_of_val(&st));
    fill(UserOutPtr::from(st.as_mut_ptr() as usize))?;
    let at = |offset: usize| st[offset / 8];
    Ok((
        at(core::mem::offset_of!(Stat, dev)),
        at(core::mem::offset_of!(Stat, ino)),
    ))
}

fn lstat(sys: &Syscall<'_>, path: &str) -> LxResult<(u64, u64)> {
    let path = c(path);
    dev_ino(|out| sys.sys_lstat(UserInPtr::from(path.as_ptr() as usize), out))
}

fn stat(sys: &Syscall<'_>, path: &str) -> LxResult<(u64, u64)> {
    let path = c(path);
    dev_ino(|out| sys.sys_stat(UserInPtr::from(path.as_ptr() as usize), out))
}

fn fstat(sys: &Syscall<'_>, fd: usize) -> LxResult<(u64, u64)> {
    dev_ino(|out| sys.sys_fstat((fd as i32).into(), out))
}

/// The names `dir` lists, without `.` and `..`.
fn names(proc: &Process, dir: &str) -> Vec<String> {
    let dir = proc.linux().lookup_inode(dir).unwrap();
    (0..)
        .map_while(|i| dir.get_entry(i).ok())
        .filter(|name| name != "." && name != "..")
        .collect()
}

#[test]
fn the_kernel_s_filesystems_are_devices_of_their_own() {
    const TOPS: [&str; 7] = ["/", "/proc", "/sys", "/dev", "/dev/shm", "/tmp", "/run"];
    let proc = a_process(45_650, "sd_tops");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move { TOPS.map(|top| stat(sys, top).map(|(dev, _)| dev)) })
    });
    let devs: Vec<u64> = got.iter().map(|dev| *dev.as_ref().unwrap()).collect();
    for (i, (a, dev_a)) in TOPS.iter().zip(&devs).enumerate() {
        assert_ne!(*dev_a, 0, "{} is device 0", a);
        for (b, dev_b) in TOPS.iter().zip(&devs).skip(i + 1) {
            assert_ne!(dev_a, dev_b, "{} and {} are one device", a, b);
        }
    }
}

#[test]
fn everything_in_dev_is_on_its_device_but_what_is_mounted_there() {
    let proc = a_process(45_651, "sd_dev");
    let entries = names(&proc, "/dev");
    assert!(entries.len() > 5, "{:?}", entries);
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let top = stat(sys, "/dev").unwrap().0;
            let each: Vec<_> = entries
                .into_iter()
                .map(|name| {
                    let dev = lstat(sys, &format!("/dev/{}", name)).map(|(dev, _)| dev);
                    (name, dev)
                })
                .collect();
            (top, each)
        })
    });
    let (top, each) = got;
    for (name, dev) in each {
        let dev = dev.unwrap();
        if name == "shm" {
            // A tmpfs of its own on Linux, and a ramfs of its own here.
            assert_ne!(dev, top, "/dev/shm is on /dev's device");
        } else {
            assert_eq!(dev, top, "/dev/{} is not on /dev's device", name);
        }
    }
}

#[test]
fn everything_in_proc_is_on_its_device() {
    let proc = a_process(45_652, "sd_proc");
    // Something for `fd` and `fdinfo` to list.
    proc.linux().add_file(UnixSocketState::new()).unwrap();
    let mut paths: Vec<String> = Vec::new();
    // By number: listed from here, `self` is no process.
    let mine = format!("/proc/{}", 45_652);
    for dir in [
        String::from("/proc"),
        mine.clone(),
        format!("{}/fd", mine),
        format!("{}/fdinfo", mine),
    ] {
        paths.extend(
            names(&proc, &dir)
                .iter()
                .map(|name| format!("{}/{}", dir, name)),
        );
        paths.push(dir);
    }
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let top = stat(sys, "/proc").unwrap().0;
            let each: Vec<_> = paths
                .into_iter()
                .map(|path| {
                    let dev = lstat(sys, &path).map(|(dev, _)| dev);
                    (path, dev)
                })
                .collect();
            (top, each)
        })
    });
    let (top, each) = got;
    assert_ne!(top, 0);
    let mut asked = 0;
    for (path, dev) in each {
        // Another test's process can be reaped between the listing and here.
        if let Ok(dev) = dev {
            assert_eq!(dev, top, "{} is not on /proc's device", path);
            asked += 1;
        }
    }
    assert!(asked > 40, "only {} names were there to ask", asked);
}

#[test]
fn a_file_is_on_the_device_of_the_filesystem_it_is_made_in() {
    let proc = a_process(45_653, "sd_files");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            ["/", "/tmp", "/run", "/dev/shm"].map(|dir| {
                let path = format!("{}/sd_f", dir.trim_end_matches('/'));
                let name = c(&path);
                let fd = sys
                    .sys_openat(
                        FileDesc::CWD,
                        UserInPtr::from(name.as_ptr() as usize),
                        0o100 | 1,
                        0o644,
                    )
                    .unwrap();
                (dir, stat(sys, dir), stat(sys, &path), fstat(sys, fd))
            })
        })
    });
    for (dir, there, made, opened) in got {
        let there = there.unwrap().0;
        assert_eq!(made.unwrap().0, there, "a file made in {}", dir);
        assert_eq!(
            opened.unwrap().0,
            there,
            "fstat of a file opened in {}",
            dir
        );
    }
}

#[test]
fn pipes_sockets_and_anonymous_objects_are_on_three_devices_of_their_own() {
    const AF_UNIX: usize = 1;
    const SOCK_STREAM: usize = 1;
    let proc = a_process(45_654, "sd_anon");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut own = Syscall {
                thread: sys.thread,
                thread_fn: sys.thread_fn,
                syscall_entry: sys.syscall_entry,
            };
            let (mut pipe, mut sockets) = ([0i32; 2], [0i32; 2]);
            sys.sys_pipe2(UserOutPtr::from(pipe.as_mut_ptr() as usize), 0)
                .unwrap();
            own.sys_socketpair(
                AF_UNIX,
                SOCK_STREAM,
                0,
                UserOutPtr::from(sockets.as_mut_ptr() as usize),
            )
            .unwrap();
            let eventfd = sys.sys_eventfd2(0, 0).unwrap();
            let epoll = sys.sys_epoll_create1(0).unwrap();
            let at = |fd: i32| fstat(sys, fd as usize).unwrap();
            (
                [at(pipe[0]), at(pipe[1])],
                [at(sockets[0]), at(sockets[1])],
                [at(eventfd as i32), at(epoll as i32)],
                [stat(sys, "/").unwrap(), stat(sys, "/proc").unwrap()],
            )
        })
    });
    let (pipes, sockets, anon, tops) = got;
    for (what, pair) in [
        ("a pipe", pipes),
        ("a socket", sockets),
        ("an eventfd and an epoll", anon),
    ] {
        assert_eq!(pair[0].0, pair[1].0, "{}: two devices", what);
    }
    let devs = [pipes[0].0, sockets[0].0, anon[0].0, tops[0].0, tops[1].0];
    for (i, a) in devs.iter().enumerate() {
        assert_ne!(*a, 0, "device {} is 0", i);
        for b in &devs[i + 1..] {
            assert_ne!(
                a, b,
                "{:?}: pipefs, sockfs, anon_inodefs, / and /proc",
                devs
            );
        }
    }
}
