//! A fork's copy of a mapping is the same memory under the same name: the
//! name and the file offset are what `/proc/<pid>/maps` shows, and a child
//! that has not exec'd yet showed every private row as anonymous memory at
//! offset 0, its own executable and libraries included.
use super::*;

fn named(vmo: Arc<VmObject>, name: &str, file_offset: usize) -> Arc<VmObject> {
    vmo.set_name(name);
    vmo.set_file_offset(file_offset);
    vmo
}

fn forked(parent: &Arc<VmAddressRegion>) -> Vec<MappingDump> {
    let child = VmAddressRegion::new_root();
    child.fork_from(parent, false).unwrap();
    child.mappings_dump()
}

/// An ordinary private copy (`fork_copy`), and the eager copy a page-cache
/// window takes -- which is what every read-only segment of an executable
/// is.
#[test]
fn a_forked_row_keeps_its_name_and_file_offset() {
    let parent = VmAddressRegion::new_root();
    let rw = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let data = named(VmObject::new_paged(2), "/bin/survey", 0xd000);
    parent.map_at(0, data, 0, 2 * PAGE_SIZE, rw).unwrap();
    let image = VmObject::new_paged(4);
    let text = VmObject::new_paged_borrowing_capped(2, image, PAGE_SIZE, 2 * PAGE_SIZE);
    let text = named(text, "/lib/ld-musl-x86_64.so.1", PAGE_SIZE);
    let rx = MMUFlags::READ | MMUFlags::EXECUTE | MMUFlags::USER;
    parent
        .map_at(4 * PAGE_SIZE, text, 0, 2 * PAGE_SIZE, rx)
        .unwrap();

    let rows = forked(&parent);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "/bin/survey");
    assert_eq!(rows[0].file_offset, 0xd000);
    assert_eq!(rows[1].name, "/lib/ld-musl-x86_64.so.1");
    assert_eq!(rows[1].file_offset, PAGE_SIZE);
}

/// Anonymous memory stays anonymous.
#[test]
fn a_forked_anonymous_row_stays_unnamed() {
    let parent = VmAddressRegion::new_root();
    let rw = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    parent
        .map_at(0, VmObject::new_paged(1), 0, PAGE_SIZE, rw)
        .unwrap();
    let rows = forked(&parent);
    assert_eq!(rows[0].name, "");
    assert_eq!(rows[0].file_offset, 0);
}
