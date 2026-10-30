//! A process that exits gives back the memory it held.
//!
//! Every page of a private mapping is a frame owned by its VMO, and the VMO
//! is owned by the mappings in the process's address space. So "all of it
//! came back" is: once the process has exited and the last handle to it is
//! gone, the VMO is gone -- and with it every frame, since `PhysFrame` frees
//! itself on drop. Asked through a `Weak` rather than the frame counter, which
//! is one machine-wide figure every test running beside this one moves too.

use super::*;
extern crate std;
use kernel_hal::MMUFlags;
use rcore_fs_ramfs::RamFS;
use zircon_object::vm::{VmObject, PAGE_SIZE};

const PAGES: usize = 16;

/// Whether `weak` is gone, allowing for a test running beside this one that
/// is holding a strong handle for a moment: anything that walks the job tree
/// (`all_live_processes`, a group signal) clones every process it passes, so a
/// reference can outlive the exit by however long that walk takes. A leak is
/// still there after a whole second.
fn gone<T: ?Sized>(weak: &Weak<T>) -> bool {
    for _ in 0..1000 {
        if weak.upgrade().is_none() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    false
}

/// A process with `PAGES` written pages, and a handle on the VMO behind them.
fn a_process_holding_memory(pid: KoID) -> (Arc<Process>, Weak<VmObject>, usize) {
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap();
    let vmo = VmObject::new_paged(PAGES);
    let weak = Arc::downgrade(&vmo);
    let addr = proc
        .vmar()
        .map(
            None,
            vmo,
            0,
            PAGES * PAGE_SIZE,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
        )
        .unwrap();
    for i in 0..PAGES {
        proc.vmar()
            .write_memory(addr + i * PAGE_SIZE, &[0xa5; 8])
            .unwrap();
    }
    (proc, weak, addr)
}

#[test]
fn an_exited_process_lets_go_of_every_page_it_wrote() {
    let (proc, vmo, _) = a_process_holding_memory(47_101);
    let process = Arc::downgrade(&proc);
    proc.exit(0);
    drop(proc);
    assert!(gone(&process), "the process itself outlived its exit");
    assert!(
        gone(&vmo),
        "its memory outlived it: the frames behind {} written pages were never freed",
        PAGES
    );
}

#[test]
fn a_forked_child_and_its_parent_both_let_go_when_they_exit() {
    let (parent, vmo, addr) = a_process_holding_memory(47_102);
    let child = Process::fork_from(&parent).unwrap();
    // The child writes every page, so each one is copied: the child holds
    // frames of its own, on top of the ones it shares with the parent.
    for i in 0..PAGES {
        child
            .vmar()
            .write_memory(addr + i * PAGE_SIZE, &[0x5a; 8])
            .unwrap();
    }
    let (p, c) = (Arc::downgrade(&parent), Arc::downgrade(&child));
    child.exit(0);
    parent.exit(0);
    drop(child);
    drop(parent);
    assert!(gone(&p), "the parent outlived its exit");
    assert!(gone(&c), "the child outlived its exit");
    assert!(gone(&vmo), "the pages the two of them wrote outlived both");
}
