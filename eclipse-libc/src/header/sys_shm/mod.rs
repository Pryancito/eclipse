//! sys/shm.h - Shared memory
use crate::types::*;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ipc_perm {
    pub __key: key_t,
    pub uid: uid_t,
    pub gid: gid_t,
    pub cuid: uid_t,
    pub cgid: gid_t,
    pub mode: c_ushort,
    pub __seq: c_ushort,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct shmid_ds {
    pub shm_perm: ipc_perm,
    pub shm_segsz: size_t,
    pub shm_atime: time_t,
    pub shm_dtime: time_t,
    pub shm_ctime: time_t,
    pub shm_cpid: pid_t,
    pub shm_lpid: pid_t,
    pub shm_nattch: c_ulong,
}

#[no_mangle]
pub unsafe extern "C" fn shmget(_key: key_t, _size: size_t, _shmflg: c_int) -> c_int {
    // Stub: always fail
    -1
}

#[no_mangle]
pub unsafe extern "C" fn shmat(_shmid: c_int, _shmaddr: *const c_void, _shmflg: c_int) -> *mut c_void {
    // Stub: always fail
    !0 as *mut c_void // (void*) -1
}

#[no_mangle]
pub unsafe extern "C" fn shmdt(_shmaddr: *const c_void) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn shmctl(_shmid: c_int, _cmd: c_int, _buf: *mut shmid_ds) -> c_int {
    -1
}
