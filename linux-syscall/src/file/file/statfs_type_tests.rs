use super::*;

#[test]
fn f_type_is_a_real_magic_not_zero() {
    let info = FsInfo {
        bsize: 4096,
        frsize: 4096,
        blocks: 1,
        bfree: 1,
        bavail: 1,
        files: 1,
        ffree: 1,
        namemax: 255,
        ftype: 0xEF53,
    };
    let st: StatFs = info.into();
    assert_ne!(st.f_type, 0, "userspace treats f_type=0 as unknown FS");
    assert_eq!(st.f_type, 0xEF53);
}

#[test]
fn proc_ftype_survives_into_statfs() {
    // systemd proc_mounted() is path_is_fs_type("/proc/", PROC_SUPER_MAGIC).
    let info = FsInfo {
        bsize: 4096,
        frsize: 4096,
        blocks: 0,
        bfree: 0,
        bavail: 0,
        files: 0,
        ffree: 0,
        namemax: 255,
        ftype: rcore_fs::vfs::PROC_SUPER_MAGIC,
    };
    let st: StatFs = info.into();
    assert_eq!(st.f_type, 0x9fa0);
}
