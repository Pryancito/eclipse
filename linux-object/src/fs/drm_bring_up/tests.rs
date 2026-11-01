//! A test build does not let `create_root_fs` rebuild the DRM globals.
//!
//! This is the pin for [`super::bring_up_drm_globals`] being a no-op under
//! `cfg(test)`: put the boot-time bring-up back and this test fails, with
//! the reason written out.

use crate::fs::devfs::drm::{build_gpu_nodes, clear_gpu_nodes_for_test, gpu_nodes};
use crate::fs::devfs::kms_emu::{self, EmuGpu};
use crate::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;

/// Leaves the node table empty however the test ends. Declared as a `let`
/// AFTER the `kms_emu::Screen`, so it drops BEFORE it and the reset happens
/// with the DRM test lock still held -- a reset that ran after the lock was
/// released could erase a table another thread's test had just built.
struct NodeTable;
impl Drop for NodeTable {
    fn drop(&mut self) {
        clear_gpu_nodes_for_test();
    }
}

/// `LinuxProcess::new` runs `create_root_fs`, and 236 call sites in this
/// binary do it, from every thread at once. If that still brought the
/// process-wide DRM state up, a registered card would be enough to give it a
/// node table -- which is what used to land on whichever DRM test was
/// running beside it.
///
/// So: a card registered and the table deliberately left empty. The bring-up
/// would fill it (the card is the only one, so `get_display_driver` picks it
/// and it becomes `card0`); a test build must leave it exactly as it was.
#[test]
fn making_a_process_does_not_rebuild_the_node_table() {
    let screen = kms_emu::attach(16, 16);
    let _table = NodeTable;
    let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-bring-up").console_at(0x01, 0x00));

    // The bring-up's own answer, so the assertion below is comparing against
    // something and not against an accident: with this card registered the
    // table is one node long.
    build_gpu_nodes();
    assert_eq!(
        gpu_nodes().len(),
        1,
        "the registered card should get a node when the table IS built"
    );

    clear_gpu_nodes_for_test();
    let _proc = LinuxProcess::new(RamFS::new(), 0);
    assert!(
        gpu_nodes().is_empty(),
        "`LinuxProcess::new` rebuilt the /dev/dri node table: boot-time DRM \
         bring-up is running once per process in the test binary, over globals \
         the DRM test holding `drm::test_globals::lock()` owns"
    );
}
