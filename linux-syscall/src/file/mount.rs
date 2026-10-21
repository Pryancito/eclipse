//! mount(2) and umount2(2)

use super::*;
use linux_object::fs::{mount_fs, umount_fs};
use linux_object::process::CAP_SYS_ADMIN;

/// Whether the caller may touch the mount table at all.
///
/// `fs/namespace.c` puts the question in one function and every mount-table
/// syscall -- `mount`, `umount`, `move_mount`, `pivot_root`, the lot -- opens
/// with it:
///
/// ```c
/// bool may_mount(void)
/// {
///         return ns_capable(current->nsproxy->mnt_ns->user_ns, CAP_SYS_ADMIN);
/// }
/// ```
///
/// Ahead of everything else, so an unprivileged caller cannot even learn from
/// the error code whether a path exists.
fn may_mount(syscall: &Syscall<'_>) -> linux_object::error::LxResult<()> {
    if syscall.linux_process().capable(CAP_SYS_ADMIN) {
        Ok(())
    } else {
        Err(LxError::EPERM)
    }
}

impl Syscall<'_> {
    /// Mount a filesystem.
    pub fn sys_mount(
        &self,
        source: UserInPtr<u8>,
        target: UserInPtr<u8>,
        fstype: UserInPtr<u8>,
        flags: usize,
        data: UserInPtr<u8>,
    ) -> SysResult {
        let source = source.as_c_str()?;
        let target = target.as_c_str()?;
        let fstype = fstype.as_c_str()?;
        let data = if data.is_null() { "" } else { data.as_c_str()? };
        info!(
            "mount: source={:?}, target={:?}, fstype={:?}, flags={:#x}",
            source, target, fstype, flags
        );
        may_mount(self)?;
        mount_fs(self.linux_process(), source, target, fstype, flags, data)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }

    /// Unmount a filesystem.
    pub fn sys_umount2(&self, target: UserInPtr<u8>, flags: usize) -> SysResult {
        let target = target.as_c_str()?;
        info!("umount2: target={:?}, flags={:#x}", target, flags);
        may_mount(self)?;
        umount_fs(target, flags)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    //! The privilege gate of the mount table, which had no test of its own.
    //!
    //! `mount(2)` and `umount2(2)` are the two calls that can change what
    //! filesystem a path leads to, and the only thing between them and an
    //! unprivileged program is `may_mount`. Nothing in this file ran: all
    //! three of its functions were at zero coverage, so a gate that answered
    //! `Ok(())` to everyone would have passed the suite.
    //!
    //! What is asked below is not only that the gate refuses, but that it
    //! refuses FIRST. An error from further in -- EINVAL for an empty
    //! filesystem type, ENODEV for one this kernel has never heard of --
    //! tells an unprivileged caller something about the system it was not
    //! allowed to ask about, and that is why Linux puts `may_mount` ahead of
    //! all of it.

    use super::*;
    use crate::on_a_thread::on_a_thread;
    use alloc::boxed::Box;
    use alloc::format;
    use alloc::string::String;
    use linux_object::process::LinuxProcess;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::{Process, ROOT_JOB};

    /// A NUL-terminated copy, which is what the pointer a syscall is handed
    /// actually points at.
    fn c(s: &str) -> String {
        format!("{}\0", s)
    }

    fn at(s: &str) -> UserInPtr<u8> {
        UserInPtr::from(s.as_ptr() as usize)
    }

    /// Root: `has_capability` answers yes to every capability for euid 0.
    fn a_root_process(pid: KoID, name: &str) -> Arc<Process> {
        let lp = LinuxProcess::new(RamFS::new(), 0);
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
    }

    /// The same process with its effective uid off root, which is the only
    /// thing `capable` looks at.
    fn an_unprivileged_process(pid: KoID, name: &str) -> Arc<Process> {
        let lp = LinuxProcess::new(RamFS::new(), 0);
        lp.set_resuid(1000, 1000, 1000).unwrap();
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
    }

    /// An umount option no kernel defines, so `check_umount_flags` refuses
    /// it -- from inside `umount_fs`, which is past the gate.
    const AN_UNKNOWN_UMOUNT_FLAG: usize = 0x8000;

    /// The three answers this process gets to three requests that are each
    /// refused somewhere DIFFERENT: an empty filesystem type (`parse_fstype`),
    /// one this kernel does not have (`parse_fstype` again, with its own
    /// errno) and an umount flag it does not know (`check_umount_flags`).
    ///
    /// All three go out on the one thread on purpose. `on_a_thread` ends the
    /// thread when its call returns, and with it the process when that was
    /// its only thread, so a second call on the same process would be asking
    /// a corpse.
    fn the_three_requests(proc: &Arc<Process>) -> (SysResult, SysResult, SysResult) {
        let (none, slash) = (c("none"), c("/"));
        let (empty, unknown) = (c(""), c("nosuchfs"));
        on_a_thread(proc, move |sys| {
            Box::pin(async move {
                // `UserInPtr` is not `Copy`, so the null `data` pointer is
                // built again for each call rather than shared.
                let empty_fstype =
                    sys.sys_mount(at(&none), at(&slash), at(&empty), 0, UserInPtr::from(0));
                let unknown_fstype =
                    sys.sys_mount(at(&none), at(&slash), at(&unknown), 0, UserInPtr::from(0));
                let unknown_flag = sys.sys_umount2(at(&slash), AN_UNKNOWN_UMOUNT_FLAG);
                (empty_fstype, unknown_fstype, unknown_flag)
            })
        })
    }

    #[test]
    fn mounting_without_cap_sys_admin_is_eperm() {
        let proc = an_unprivileged_process(45_540, "mount-unprivileged");
        let (none, slash, fstype) = (c("none"), c("/"), c("proc"));
        // `proc` is one of the types `is_virtual_fstype` names, so this
        // request is not refusable on its own account: what is turned away
        // is the caller. And because the gate answers first, nothing is
        // written to the mount table on the way out.
        let got = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                sys.sys_mount(at(&none), at(&slash), at(&fstype), 0, UserInPtr::from(0))
            })
        });

        assert_eq!(
            got,
            Err(LxError::EPERM),
            "an unprivileged mount was not refused"
        );
    }

    #[test]
    fn unmounting_without_cap_sys_admin_is_eperm() {
        let proc = an_unprivileged_process(45_541, "umount-unprivileged");
        let slash = c("/");
        let got = on_a_thread(&proc, move |sys| {
            Box::pin(async move { sys.sys_umount2(at(&slash), 0) })
        });

        assert_eq!(
            got,
            Err(LxError::EPERM),
            "an unprivileged umount2 was not refused"
        );
    }

    #[test]
    fn the_gate_answers_before_the_request_is_looked_at() {
        // The root half is this lot's control as well: a gate stuck at EPERM
        // would fail these three and not the two tests above.
        let (empty, unknown, flag) = the_three_requests(&a_root_process(45_542, "mount-as-root"));
        assert_eq!(
            empty,
            Err(LxError::EINVAL),
            "root did not reach parse_fstype"
        );
        assert_eq!(
            unknown,
            Err(LxError::ENODEV),
            "root did not reach parse_fstype"
        );
        assert_eq!(
            flag,
            Err(LxError::EINVAL),
            "root did not reach check_umount_flags"
        );

        // The same three requests, from a caller that may not mount, get the
        // same answer every time, and it is the gate's.
        let (empty, unknown, flag) =
            the_three_requests(&an_unprivileged_process(45_543, "mount-as-other"));
        assert_eq!(
            empty,
            Err(LxError::EPERM),
            "an empty fstype answered EINVAL to a caller that may not mount"
        );
        assert_eq!(
            unknown,
            Err(LxError::EPERM),
            "an unknown fstype answered ENODEV to a caller that may not mount"
        );
        assert_eq!(
            flag,
            Err(LxError::EPERM),
            "an unknown umount flag answered EINVAL to a caller that may not mount"
        );
    }
}
