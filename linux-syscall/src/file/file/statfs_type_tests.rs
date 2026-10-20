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
    };
    let st: StatFs = info.into();
    assert_ne!(st.f_type, 0, "userspace treats f_type=0 as unknown FS");
    assert_eq!(st.f_type, 0xEF53);
}
