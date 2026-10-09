use super::*;
use crate::ipc::{sem_lookup, sem_register, sem_unregister, SemArray};
use rcore_fs_ramfs::RamFS;

#[test]
fn a_set_named_by_id_from_another_process_is_found_and_remembered() {
    let _guard = crate::ipc::sem_test_globals::lock();
    // The set exists system-wide; this process never called semget on it.
    let array = SemArray::get_or_create(0, 1, 0o1000 | 0o666, 0, 0, &[]).unwrap();
    let id = sem_register(&array).unwrap();
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        0x5e11,
        "stranger",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let lp = proc.linux();
    let found = lp
        .semaphores_get(id)
        .expect("a system-wide id names the set anywhere");
    assert!(Arc::ptr_eq(&found, &array));
    // ...and it is now this process's set too, so a SEM_UNDO record left
    // on it has something to replay against at exit.
    assert!(lp.inner.lock().semaphores.get(id).is_some());
    // Retired with IPC_RMID, the id names nothing to anyone, the
    // holder included: its next semop is EINVAL, as in Linux.
    sem_unregister(id);
    assert!(sem_lookup(id).is_none());
    assert!(
        lp.semaphores_get(id).is_none(),
        "a removed set stayed reachable by id through the holder's table"
    );
    assert!(lp.semaphores_get(id + 1).is_none());
    // The table still holds the object, for the SEM_UNDO replay at exit.
    assert!(lp.inner.lock().semaphores.get(id).is_some());
}
