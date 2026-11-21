//! `/dev/full` (char 1:7): reads are zeros, every write is `ENOSPC`. It was
//! missing, so a program that tests its full-disk path with it got `ENOENT`.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const O_RDWR: usize = 2;

#[test]
fn dev_full_reads_zeros_and_refuses_every_write_with_enospc() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        45_750,
        "full",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let path = b"/dev/full\0";
            let fd = FileDesc::from(
                sys.sys_openat(
                    FileDesc::CWD,
                    UserInPtr::from(path.as_ptr() as usize),
                    O_RDWR,
                    0,
                )
                .expect("/dev/full is there") as i32,
            );
            let mut buf = [0xffu8; 8];
            assert_eq!(
                sys.sys_read(fd, UserOutPtr::from(buf.as_mut_ptr() as usize), 8)
                    .await,
                Ok(8)
            );
            assert_eq!(buf, [0u8; 8]);
            let bytes = b"x";
            assert_eq!(
                sys.sys_write(fd, UserInPtr::from(bytes.as_ptr() as usize), 1)
                    .await,
                Err(LxError::ENOSPC)
            );
            let meta = sys
                .linux_process()
                .get_file(fd)
                .unwrap()
                .metadata()
                .unwrap();
            assert_eq!(
                meta.rdev,
                rcore_fs::vfs::make_rdev(1, 7),
                "/dev/full is not 1:7"
            );
        })
    });
}
