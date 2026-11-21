//! The seals of a memfd that were stored but not kept, and who may add one.
//!
//! `F_SEAL_GROW`: nothing may take the file past the size it had when the
//! seal went on.
//!
//! Linux checks the seal in `shmem_write_begin` (a write that ends past
//! `i_size` is `EPERM`) and in `shmem_fallocate` (an allocation that ends
//! past `i_size` is `EPERM`). The seal word was stored and reported here,
//! but only `ftruncate` looked at it, so a write past the end grew a file
//! its owner had been promised would never grow -- the promise a receiver
//! of a sealed buffer relies on to map it without a `SIGBUS`.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::fs::{File, OpenFlags, F_SEAL_EXEC, F_SEAL_GROW, F_SEAL_WRITE};
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const F_ADD_SEALS: usize = 1024 + 9;
const MFD_ALLOW_SEALING: usize = 0x0002;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A sealable memfd holding `len` bytes.
async fn a_memfd_of(sys: &Syscall<'_>, len: usize) -> FileDesc {
    let name = b"grow-sealed\0";
    let fd = FileDesc::from(
        sys.sys_memfd_create(UserInPtr::from(name.as_ptr() as usize), MFD_ALLOW_SEALING)
            .unwrap() as i32,
    );
    assert_eq!(sys.sys_ftruncate(fd, len), Ok(0));
    fd
}

#[test]
fn a_write_past_the_end_of_a_grow_sealed_memfd_is_eperm_and_one_inside_is_not() {
    let proc = a_process(45_730, "grow-write");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = a_memfd_of(sys, 8).await;
            assert_eq!(
                sys.sys_fcntl(fd, F_ADD_SEALS, F_SEAL_GROW as usize).await,
                Ok(0)
            );
            let bytes = [7u8; 8];
            let buf = || UserInPtr::from(bytes.as_ptr() as usize);

            assert_eq!(sys.sys_pwrite(fd, buf(), 8, 0).await, Ok(8));
            assert_eq!(sys.sys_pwrite(fd, buf(), 4, 4).await, Ok(4));
            assert_eq!(
                sys.sys_pwrite(fd, buf(), 8, 4).await,
                Err(LxError::EPERM),
                "a write that ends past the sealed size grew the file"
            );
            assert_eq!(
                sys.sys_pwrite(fd, buf(), 1, 8).await,
                Err(LxError::EPERM),
                "an append to a grow-sealed memfd went through"
            );
            let lp = sys.linux_process();
            let size = lp.get_file(fd).unwrap().metadata().unwrap().size;
            assert_eq!(size, 8, "the refused writes still moved the size");
        })
    });
}

#[test]
fn fallocate_past_the_end_of_a_grow_sealed_memfd_is_eperm() {
    let proc = a_process(45_731, "grow-falloc");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = a_memfd_of(sys, 4096).await;
            assert_eq!(
                sys.sys_fcntl(fd, F_ADD_SEALS, F_SEAL_GROW as usize).await,
                Ok(0)
            );
            assert_eq!(sys.sys_fallocate(fd, 0, 0, 4096), Ok(0));
            assert_eq!(
                sys.sys_fallocate(fd, 0, 0, 8192),
                Err(LxError::EPERM),
                "fallocate grew a grow-sealed memfd"
            );
        })
    });
}

#[test]
fn an_unsealed_memfd_still_grows_and_a_write_seal_still_refuses_writes() {
    // The control: without the seal the same calls grow the file, so the
    // refusals above are the seal's and not some other limit's.
    let proc = a_process(45_732, "grow-free");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = a_memfd_of(sys, 8).await;
            let bytes = [7u8; 8];
            assert_eq!(
                sys.sys_pwrite(fd, UserInPtr::from(bytes.as_ptr() as usize), 8, 4)
                    .await,
                Ok(8)
            );
            assert_eq!(sys.sys_fallocate(fd, 0, 0, 8192), Ok(0));
            // F_SEAL_WRITE alone: writes refused, as before.
            assert_eq!(
                sys.sys_fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE as usize).await,
                Ok(0)
            );
            assert_eq!(
                sys.sys_pwrite(fd, UserInPtr::from(bytes.as_ptr() as usize), 1, 0)
                    .await,
                Err(LxError::EPERM)
            );
        })
    });
}

#[test]
fn only_a_descriptor_open_for_writing_may_add_a_seal() {
    // `memfd_add_seals` refuses a descriptor without FMODE_WRITE: the
    // read-only copy a peer receives must not be able to seal the owner's
    // buffer against the owner. Reading the seals needs no such thing.
    let proc = a_process(45_733, "seal-rdonly");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = a_memfd_of(sys, 8).await;
            let lp = sys.linux_process();
            let inode = lp.get_file(fd).unwrap().inode();
            let ro: i32 = lp
                .add_file(File::new(inode, OpenFlags::RDONLY, "memfd:ro".into()))
                .unwrap()
                .into();
            let ro = FileDesc::from(ro);
            assert_eq!(
                sys.sys_fcntl(ro, F_ADD_SEALS, F_SEAL_GROW as usize).await,
                Err(LxError::EPERM),
                "a read-only descriptor sealed the memfd"
            );
            assert_eq!(sys.sys_fcntl(ro, F_ADD_SEALS + 1, 0).await, Ok(0));
            assert_eq!(
                sys.sys_fcntl(fd, F_ADD_SEALS, F_SEAL_GROW as usize).await,
                Ok(0)
            );
            assert_eq!(
                sys.sys_fcntl(ro, F_ADD_SEALS + 1, 0).await,
                Ok(F_SEAL_GROW as usize)
            );
        })
    });
}

#[test]
fn f_seal_exec_is_taken_and_freezes_only_the_execute_bits() {
    // Linux 6.3's F_SEAL_EXEC: `shmem_setattr` refuses a chmod that changes
    // any of the three x bits, and lets the rest of the mode change.
    let proc = a_process(45_734, "seal-exec");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = a_memfd_of(sys, 8).await;
            assert_eq!(sys.sys_fchmod(fd, 0o755), Ok(0));
            assert_eq!(
                sys.sys_fcntl(fd, F_ADD_SEALS, F_SEAL_EXEC as usize).await,
                Ok(0),
                "F_SEAL_EXEC is a Linux seal and was refused"
            );
            assert_eq!(
                sys.sys_fchmod(fd, 0o644),
                Err(LxError::EPERM),
                "an exec-sealed memfd lost its execute bits"
            );
            assert_eq!(
                sys.sys_fchmod(fd, 0o711),
                Ok(0),
                "a change that keeps the execute bits was refused"
            );
        })
    });
}
