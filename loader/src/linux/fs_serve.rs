//! [`ECLIPSE_FS_SERVE`]: a filesystem that a process serves, mounted like
//! any other. The kernel's side of it is `linux_object::fs::served`.

use alloc::sync::Arc;
use linux_object::error::LxError;
use linux_object::process::ProcessExt;
use zircon_object::ipc::Fifo;
use zircon_object::object::{Handle, KernelObject, Rights, Signal};
use zircon_object::task::Process;
use zircon_object::vm::VmObject;
use zircon_object::ZxError;

/// `eclipse_fs_serve(fifo, window, names, names_len, flags, kept)`: mount
/// a filesystem that this process serves. `fifo` is the end of a FIFO of
/// 32-byte messages the kernel keeps and `window` a VMO the names and the
/// data of each request pass through, a slot of 68 KiB for each request in
/// flight, up to 16 (`linux_object::fs::served::wire` has the messages).
/// `names` is `names_len` bytes: where to mount (an absolute path), then
/// what the mount table shows as the mount's source and as its type, a NUL
/// between each two. `flags`: 1 when a name matches an entry whatever the
/// case of its ASCII letters, 2 for a read-only mount, 4 to take a mount
/// over and make none. `kept` is a VMO the mount holds for its servers, or
/// 0: what a server must not lose when it dies.
///
/// The process answers each message on the other end of the FIFO, and sees
/// that end closed when the filesystem is unmounted. When the process is
/// gone the mount stays: its requests wait ten seconds for the next
/// server, and fail after that until one comes.
///
/// The next server is a process that makes this call with the same place,
/// source, type and flags: it is given the mount as it is, and the answer
/// is a handle to the `kept` VMO of the mount's first server (1 when that
/// one gave none) instead of 0. Its own `kept` is dropped.
///
/// All three handles go to the kernel, whatever the answer.
///
/// `ACCESS_DENIED` to a process that may not `mount(2)`, `NOT_FOUND` when
/// there is no such directory or, with flag 4, no such mount,
/// `ALREADY_BOUND` when the mount has a server, `INVALID_ARGS` for names
/// that are not three, a relative path, a window that does not hold a
/// slot, a FIFO of another message size or other flags than the mount's.
pub const ECLIPSE_FS_SERVE: u32 = super::ECLIPSE_GATEWAY_EXT + 13;

/// The longest `names`: a path and two words.
const NAMES_MAX: usize = 4096 + 256;

pub(super) fn fs_serve(proc: &Arc<Process>, args: &[usize; 6]) -> isize {
    match serve(proc, args) {
        Ok(answer) => answer as isize,
        Err(e) => e as isize,
    }
}

fn serve(proc: &Arc<Process>, args: &[usize; 6]) -> Result<u32, ZxError> {
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::READ | Rights::WRITE);
    let window =
        proc.get_object_with_rights::<VmObject>(args[1] as u32, Rights::READ | Rights::WRITE);
    let kept = match args[5] as u32 {
        0 => Ok(None),
        handle => proc
            .get_object_with_rights::<VmObject>(handle, Rights::READ | Rights::WRITE)
            .map(Some),
    };
    for handle in [args[0], args[1], args[5]] {
        if handle != 0 {
            let _ = proc.remove_handle(handle as u32);
        }
    }
    let (fifo, window, kept) = (fifo?, window?, kept?);
    if args[3] == 0 || args[3] > NAMES_MAX {
        return Err(ZxError::INVALID_ARGS);
    }
    let names = kernel_hal::user::UserInPtr::<u8>::from(args[2])
        .read_array(args[3])
        .map_err(|_| ZxError::INVALID_ARGS)?;
    let names = core::str::from_utf8(&names).map_err(|_| ZxError::INVALID_ARGS)?;
    let mut names = names.split('\0');
    let (Some(target), Some(source), Some(fstype), None) =
        (names.next(), names.next(), names.next(), names.next())
    else {
        return Err(ZxError::INVALID_ARGS);
    };
    let served = linux_object::fs::served::serve(
        proc.linux(),
        fifo.clone(),
        window,
        kept,
        target,
        source,
        fstype,
        args[4],
    )
    .map_err(|e| match e {
        LxError::EPERM => ZxError::ACCESS_DENIED,
        LxError::ENOENT | LxError::ENOTDIR => ZxError::NOT_FOUND,
        LxError::EBUSY => ZxError::ALREADY_BOUND,
        _ => ZxError::INVALID_ARGS,
    })?;
    let answer = match (&served.kept, served.taken_over) {
        (Some(kept), _) => proc.add_handle(Handle::new(kept.clone(), Rights::DEFAULT_VMO)),
        (None, true) => 1,
        (None, false) => 0,
    };
    // Nothing reads the FIFO while no request is out, so nothing would
    // notice the server going: this does, and the mount's wait for the
    // next one starts when it went.
    let (fs, turn) = (served.fs, served.turn);
    kernel_hal::thread::spawn(async move {
        let wire: Arc<dyn KernelObject> = fifo;
        // The server went, or the mount did, and then this end must go
        // for the server to see it.
        wire.wait_signal(Signal::PEER_CLOSED | linux_object::fs::served::SERVER_LEFT)
            .await;
        drop(wire);
        fs.server_left(turn);
    });
    Ok(answer)
}
