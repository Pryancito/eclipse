//! mount(2) and umount2(2)

use super::*;
use alloc::string::String;
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
        // `copy_mount_string`: a NULL source or type is no string, not a bad
        // address. A remount, a bind, a move and a change of propagation are
        // asked for that way (busybox's `mount --bind` passes no type), and
        // every one of them was `EFAULT`.
        let source = if source.is_null() {
            None
        } else {
            Some(source.as_c_str()?)
        };
        let target = target.as_c_str()?;
        let fstype = if fstype.is_null() {
            ""
        } else {
            fstype.as_c_str()?
        };
        let data = if data.is_null() { "" } else { data.as_c_str()? };
        info!(
            "mount: source={:?}, target={:?}, fstype={:?}, flags={:#x}",
            source, target, fstype, flags
        );
        may_mount(self)?;
        let proc = self.linux_process();
        // `do_mount` looks the target up before anything else is read.
        let target = mount_path(proc, target, true)?;
        // What a bind or a move takes is a path too (`do_loopback`,
        // `do_move_mount_old`), and an empty one is no name at all. A device
        // is opened from the caller's directory further in, and its line
        // keeps the name it was given, as Linux keeps `fc->source`.
        let path_source;
        let source = match source {
            Some(name) if flags & MS_REMOUNT == 0 && flags & (MS_BIND | MS_MOVE) != 0 => {
                if name.is_empty() {
                    return Err(LxError::EINVAL);
                }
                path_source = mount_path(proc, name, true)?;
                Some(path_source.as_str())
            }
            source => source,
        };
        mount_fs(proc, source, &target, fstype, flags, data)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }

    /// Unmount a filesystem.
    pub fn sys_umount2(&self, target: UserInPtr<u8>, flags: usize) -> SysResult {
        let target = target.as_c_str()?;
        info!("umount2: target={:?}, flags={:#x}", target, flags);
        may_mount(self)?;
        // `ksys_umount`: the word before the path, so an option this kernel
        // does not know is `EINVAL` whether or not the path is there.
        check_umount_flags(flags)?;
        let follow = flags & UMOUNT_NOFOLLOW == 0;
        let target = mount_path(self.linux_process(), target, follow)?;
        umount_fs(&target, flags)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }
}

/// The path `mount(2)` and `umount2(2)` act on, as `user_path_at` finds it:
/// from the caller's working directory when it is relative, through the
/// symbolic links on the way (and the last one only when `follow`), and
/// named from the root by the directories the walk really entered, which is
/// how the mount table names a mount point (`d_path`).
///
/// Both took the string as it came, from the kernel's root: a relative path
/// was looked up from `/`, a link was not followed, and a `..` stayed in the
/// name. So `mount --bind dir b` run in `/run` looked for `/b`, a bind onto
/// or of a link was `ENOTDIR`, `umount` through one was `EINVAL`, a tmpfs on
/// `c` was listed as `/c`, and a bind onto `c/../b` under that name.
fn mount_path(proc: &LinuxProcess, path: &str, follow: bool) -> LxResult<String> {
    let (_, walked) = proc.lookup_inode_walked(FileDesc::CWD, path, follow)?;
    Ok(walked.path())
}

/// Where `mount(2)` and `umount2(2)` find the paths they are given, and in
/// what order they look at them.
#[cfg(test)]
mod path_tests;

/// What a bind takes from the call that makes it.
#[cfg(test)]
mod bind_tests;

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
    use alloc::vec;
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

    /// A target no other test names, so looking for it in the mount table --
    /// which every test in this binary shares -- cannot race with anything.
    const A_TARGET_OF_ITS_OWN: &str = "/mount-gate-probe";

    #[test]
    fn a_refused_mount_leaves_no_trace_in_the_mount_table() {
        // The errno alone does not say the mount did not happen. Asking
        // `may_mount` AFTER the work returns EPERM just the same, having
        // mounted the filesystem on the way -- which is the whole of the
        // bug. So this asks what userspace would ask: `/proc/mounts`, where
        // `register_mount` publishes a mount that took place.
        let proc = an_unprivileged_process(45_544, "mount-no-trace");
        let (none, target, fstype) = (c("none"), c(A_TARGET_OF_ITS_OWN), c("proc"));
        let (got, mounts) = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let got = sys.sys_mount(at(&none), at(&target), at(&fstype), 0, UserInPtr::from(0));
                let inode = sys.linux_process().lookup_inode("/proc/mounts").unwrap();
                let mut buf = vec![0u8; 8192];
                let read = inode.read_at(0, &mut buf).unwrap();
                buf.truncate(read);
                (got, String::from_utf8_lossy(&buf).into_owned())
            })
        });

        assert_eq!(
            got,
            Err(LxError::EPERM),
            "an unprivileged mount was not refused"
        );
        assert!(
            !mounts.contains(A_TARGET_OF_ITS_OWN),
            "a mount the gate refused was carried out anyway and published \
             in /proc/mounts:\n{}",
            mounts
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

    /// What Linux 6.18 answered to the same calls, each with NULL where a
    /// string goes. Every one was `EFAULT` here, so busybox's `mount --bind`
    /// failed with "Bad address".
    #[test]
    fn a_null_source_or_type_is_no_string_not_a_bad_address() {
        const MS_REMOUNT: usize = 32;
        const MS_BIND: usize = 4096;
        const MS_MOVE: usize = 8192;
        let proc = a_root_process(45_545, "mount-null");
        // The target is looked up first, as `do_mount` does, so it has to be
        // there for the rest of each request to be read.
        proc.linux()
            .root_inode()
            .create("mount-null-probe", rcore_fs::vfs::FileType::Dir, 0o755)
            .unwrap();
        let (target, fstype, source) = (c("/mount-null-probe"), c("tmpfs"), c("/tmp"));
        let (slash, vfat) = (c("/"), c("vfat"));
        let null = || UserInPtr::<u8>::from(0);
        let got = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                [
                    // `do_loopback` and `do_move_mount_old` want a name.
                    sys.sys_mount(null(), at(&target), null(), MS_BIND, null()),
                    sys.sys_mount(null(), at(&target), null(), MS_MOVE, null()),
                    // A new mount with no type at all.
                    sys.sys_mount(at(&source), at(&target), null(), 0, null()),
                    // A remount reads neither; this target is mounted nowhere.
                    sys.sys_mount(null(), at(&target), null(), MS_REMOUNT, null()),
                    // And the type of a bind is not looked at.
                    sys.sys_mount(at(&source), at(&target), null(), MS_BIND, null()),
                    // A pseudo-filesystem with no source, which Linux shows as
                    // `none`.
                    sys.sys_mount(null(), at(&target), at(&fstype), 0, null()),
                    // A filesystem on a device with none named: "No source
                    // specified" (`get_tree_bdev`), not a lookup of "".
                    sys.sys_mount(null(), at(&slash), at(&vfat), 0, null()),
                ]
            })
        });
        let einval: SysResult = Err(LxError::EINVAL);
        assert_eq!(
            got[..4],
            [einval.clone(), einval.clone(), einval.clone(), einval]
        );
        assert_ne!(got[4], Err(LxError::EFAULT), "a bind with no type");
        assert_eq!(got[5], Ok(0), "a tmpfs with no source");
        assert_eq!(got[6], Err(LxError::EINVAL), "a vfat with no source");
    }
}
