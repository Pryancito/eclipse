//! Whether a forking parent may put `aspace_lock` down for the copy.
//!
//! `new_child` holds `aspace_lock` -- an IRQ-off spinlock -- to freeze the
//! parent's address-space layout while `VmAddressRegion::fork_from` walks a
//! snapshot of it. Across the copy that is tens of ms with the forking CPU's
//! interrupts off, so the copy arm drops it when
//! [`alone_in_its_address_space`] says there is provably nobody to race.
//!
//! That predicate is therefore the whole safety argument, and it has two
//! halves for two different races. Dropping either half reads as a
//! simplification and is a silent memory corruption in the child: the fork
//! walks a snapshot that something reshaped underneath it, and the child
//! materializes an address space that never existed. These pin both.
//!
//! Fixed pids live in 94_6xx here; they are a global table, so every file
//! that pins one has its own range.

use super::*;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

/// A live Linux process with one thread, in the root job.
fn a_process(pid: KoID) -> Arc<Process> {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "aspace",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

/// The ordinary fork: a single-threaded parent nobody shares with. This is
/// the case the early drop exists for -- a shell, a script, a `Command` --
/// and it is the common one, so if this answered `false` the change would
/// buy nothing.
#[test]
fn un_proceso_de_un_solo_hilo_y_sin_compartir_esta_solo() {
    let proc = a_process(94_601);
    assert_eq!(proc.thread_count(), 1);
    assert!(!proc.shares_address_space());
    assert!(alone_in_its_address_space(&proc));
}

/// The race the lock was written for: another thread of the SAME process
/// calling mmap/mprotect/munmap while the fork copies. labwc is this parent
/// -- llvmpipe's JIT threads reshape mappings continuously while wlroots
/// forks Xwayland -- and it is where dropping the lock would corrupt.
#[test]
fn un_proceso_con_un_segundo_hilo_no_esta_solo() {
    let proc = a_process(94_602);
    let _second = Thread::create_linux(&proc).unwrap();
    assert_eq!(proc.thread_count(), 2);
    assert!(
        !alone_in_its_address_space(&proc),
        "con un segundo hilo vivo, ese hilo puede remodelar el layout \
         mientras fork_from camina su foto"
    );
}

/// The other half, and the one a "`thread_count() == 1` is enough" reading
/// loses: a vforked child runs in the PARENT'S address space, as its own
/// process with its own threads. The parent can be single-threaded and still
/// have somebody able to reshape it, so the count alone says nothing.
#[test]
fn un_padre_cuyo_espacio_comparte_otro_proceso_no_esta_solo() {
    let parent = a_process(94_603);
    let sharer = Process::create_sharing_address_space(
        &ROOT_JOB,
        "sharer",
        &parent,
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    Thread::create_linux(&sharer).unwrap();

    assert_eq!(parent.thread_count(), 1, "el padre sigue teniendo un hilo");
    assert!(
        parent.shares_address_space(),
        "prestar el espacio tiene que verse desde el PADRE, no solo desde \
         quien lo toma prestado"
    );
    assert!(
        !alone_in_its_address_space(&parent),
        "un hilo, pero otro proceso en el mismo espacio: contar hilos no basta"
    );
}
