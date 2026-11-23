//! `read_memory`/`write_memory`: an unmapped address and the vDSO.

use super::*;

#[test]
fn an_unmapped_address_is_not_found_and_the_vdso_is_not_written() {
    let root = VmAddressRegion::new_root();
    let mut buf = [0u8; 8];
    assert_eq!(
        root.read_memory(root.addr(), &mut buf),
        Err(ZxError::NOT_FOUND)
    );
    assert_eq!(
        root.write_memory(root.addr(), &buf),
        Err(ZxError::NOT_FOUND)
    );

    let vdso = VmObject::new_paged(1);
    crate::task::register_vdso_vmo(vdso.id());
    let at = root
        .map(
            None,
            vdso.clone(),
            0,
            PAGE_SIZE,
            MMUFlags::READ | MMUFlags::EXECUTE,
        )
        .unwrap();
    assert_eq!(root.write_memory(at, &buf), Err(ZxError::ACCESS_DENIED));
    assert_eq!(root.read_memory(at, &mut buf), Ok(8));

    let plain = VmObject::new_paged(1);
    let at = root
        .map(None, plain, 0, PAGE_SIZE, MMUFlags::READ | MMUFlags::WRITE)
        .unwrap();
    assert_eq!(root.write_memory(at, &[1, 2, 3]), Ok(3));
}
