//! `open` of a FIFO as a process makes it: it waits for the other end, a
//! kill ends the wait, and what it opens is an end of a pipe, not the node.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::time::Duration;
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileSystem;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_NONBLOCK: usize = 0o4000;
const S_IFIFO: usize = 0o010_000;

fn a_process(pid: KoID, fs: Arc<RamFS>) -> Arc<Process> {
    let lp = LinuxProcess::new(fs, 0);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "fifo", lp).unwrap()
}

fn c(path: &str) -> String {
    alloc::format!("{}\0", path)
}

fn mkfifo(sys: &Syscall<'_>, path: &str) {
    let path = c(path);
    sys.sys_mknodat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        S_IFIFO | 0o600,
        0,
    )
    .unwrap();
}

/// Make syscall `num` the way userspace makes it, through the table, as
/// `dispatch_tests` does.
async fn by_number(sys: &Syscall<'_>, num: u32, args: [usize; 6]) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.syscall(num, args).await
}

async fn open(sys: &Syscall<'_>, path: &str, flags: usize) -> LxResult<FileDesc> {
    let path = c(path);
    let fd = sys
        .sys_openat_waiting(
            FileDesc::CWD,
            UserInPtr::from(path.as_ptr() as usize),
            flags,
            0,
        )
        .await?;
    Ok(FileDesc::from(fd as i32))
}

async fn read(sys: &Syscall<'_>, fd: FileDesc) -> LxResult<Vec<u8>> {
    let mut buf = [0u8; 16];
    let n = sys
        .linux_process()
        .get_file_like(fd)?
        .read(&mut buf)
        .await?;
    Ok(buf[..n].to_vec())
}

fn write(sys: &Syscall<'_>, fd: FileDesc, bytes: &[u8]) -> LxResult<usize> {
    sys.linux_process().get_file_like(fd)?.write(bytes)
}

/// `open(O_RDONLY)`, made through the table, waits for a writer, and the
/// two ends it and the writer get are one pipe: the bytes go from one to
/// the other and not into the node.
#[test]
fn a_blocking_open_of_a_fifo_waits_for_the_other_end() {
    let proc = a_process(46_411, RamFS::new());
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            mkfifo(sys, "/f");
            let path = c("/f");
            let at_cwd = -100isize as usize;
            let args = [at_cwd, path.as_ptr() as usize, O_RDONLY, 0, 0, 0];
            let reader = by_number(sys, Sys::OPENAT as u32, args);
            futures::pin_mut!(reader);
            let waited = futures::poll!(reader.as_mut()).is_pending();
            let w = open(sys, "/f", O_WRONLY).await.unwrap();
            let r = FileDesc::from(reader.await as i32);
            assert_eq!(write(sys, w, b"hi"), Ok(2));
            let node = sys.linux_process().lookup_inode("/f").unwrap();
            (waited, read(sys, r).await, node.metadata().unwrap().size)
        })
    });
    assert_eq!(got, (true, Ok(b"hi".to_vec()), 0));
}

/// A kill ends the wait, and the end it was to open goes with it: a writer
/// that will not wait finds no reader.
#[test]
fn a_kill_ends_a_blocking_open_of_a_fifo() {
    let fs = RamFS::new();
    let proc = a_process(46_412, fs.clone());
    let killer = proc.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        killer.exit(137);
    });
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            mkfifo(sys, "/f");
            open(sys, "/f", O_RDONLY).await
        })
    });
    assert_eq!(got, Err(LxError::EINTR));
    let node = fs.root_inode().find("f").unwrap();
    let writer = Pipe::open_fifo(node, OpenFlags::WRONLY | OpenFlags::NON_BLOCK);
    assert_eq!(writer.map(|_| ()), Err(LxError::ENXIO));
}

/// `/proc/self/fd/N` of a FIFO's end is the FIFO again: it is named by its
/// path, and opening it is a new end, here the writer of a reader that was
/// opened first.
#[test]
fn a_fifo_reopened_through_proc_self_fd_is_a_new_end() {
    let proc = a_process(46_413, RamFS::new());
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            mkfifo(sys, "/f");
            let r = open(sys, "/f", O_RDONLY | O_NONBLOCK).await.unwrap();
            let link = alloc::format!("/proc/self/fd/{}", i32::from(r));
            let w = open(sys, &link, O_WRONLY | O_NONBLOCK).await.unwrap();
            assert_eq!(write(sys, w, b"x"), Ok(1));
            let mut name = [0u8; 16];
            let path = c(&alloc::format!("/proc/self/fd/{}", i32::from(w)));
            let n = sys
                .sys_readlinkat(
                    FileDesc::CWD,
                    UserInPtr::from(path.as_ptr() as usize),
                    UserOutPtr::from(name.as_mut_ptr() as usize),
                    name.len(),
                )
                .unwrap();
            (read(sys, r).await, name[..n].to_vec())
        })
    });
    assert_eq!(got, (Ok(b"x".to_vec()), b"/f".to_vec()));
}
