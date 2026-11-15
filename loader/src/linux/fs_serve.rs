//! [`ECLIPSE_FS_SERVE`]: a filesystem that a process serves, mounted like
//! any other. The kernel's side of it is `linux_object::fs::served`.

use alloc::sync::Arc;
use linux_object::error::LxError;
use linux_object::process::ProcessExt;
use zircon_object::ipc::Fifo;
use zircon_object::object::Rights;
use zircon_object::task::Process;
use zircon_object::vm::VmObject;
use zircon_object::ZxError;

/// `eclipse_fs_serve(fifo, window, names, names_len, flags)`: mount a
/// filesystem that this process serves. `fifo` is the end of a FIFO of
/// 32-byte messages the kernel keeps and `window` a VMO the names and the
/// data of each request pass through, a slot of 68 KiB for each request in
/// flight, up to 16 (`linux_object::fs::served::wire` has the messages).
/// `names` is `names_len` bytes: where to mount (an absolute path), then
/// what the mount table shows as the mount's source and as its type, a NUL
/// between each two. `flags`: 1 when a name matches an entry whatever the
/// case of its ASCII letters.
///
/// The mount is read-only. The process answers each message on the other
/// end of the FIFO, and sees that end closed when the filesystem is
/// unmounted; when the process is gone, what the kernel had not read yet is
/// an I/O error. Both handles go to the kernel, whatever the answer.
///
/// `ACCESS_DENIED` to a process that may not `mount(2)`, `NOT_FOUND` when
/// there is no such directory, `INVALID_ARGS` for names that are not three,
/// a relative path, a window that does not hold a slot or a FIFO of
/// another message size.
pub const ECLIPSE_FS_SERVE: u32 = super::ECLIPSE_GATEWAY_EXT + 13;

/// The longest `names`: a path and two words.
const NAMES_MAX: usize = 4096 + 256;

pub(super) fn fs_serve(proc: &Arc<Process>, args: &[usize; 6]) -> isize {
    match serve(proc, args) {
        Ok(()) => 0,
        Err(e) => e as isize,
    }
}

fn serve(proc: &Arc<Process>, args: &[usize; 6]) -> Result<(), ZxError> {
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::READ | Rights::WRITE);
    let window =
        proc.get_object_with_rights::<VmObject>(args[1] as u32, Rights::READ | Rights::WRITE);
    let _ = proc.remove_handle(args[0] as u32);
    let _ = proc.remove_handle(args[1] as u32);
    let (fifo, window) = (fifo?, window?);
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
    linux_object::fs::served::serve(proc.linux(), fifo, window, target, source, fstype, args[4])
        .map_err(|e| match e {
            LxError::EPERM => ZxError::ACCESS_DENIED,
            LxError::ENOENT | LxError::ENOTDIR => ZxError::NOT_FOUND,
            _ => ZxError::INVALID_ARGS,
        })
}
