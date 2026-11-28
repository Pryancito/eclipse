//! [`ECLIPSE_MOUNT_HELP`]: the process that makes the mounts `mount(2)`
//! asks for of a filesystem the kernel has no code for. The kernel's side
//! of it is `linux_object::fs::served::helper`.

use alloc::sync::Arc;
use linux_object::error::LxError;
use linux_object::process::ProcessExt;
use zircon_object::ipc::Fifo;
use zircon_object::object::{KernelObject, Rights, Signal};
use zircon_object::task::Process;
use zircon_object::vm::VmObject;
use zircon_object::ZxError;

/// `eclipse_mount_help(fifo, window)`: this process makes the mounts of
/// FAT (`vfat`) that `mount(2)` is asked for. `fifo` and `window` are as
/// a filesystem server's ([`super::fs_serve::ECLIPSE_FS_SERVE`]); each
/// request is one mount (`linux_object::fs::served::helper` has it), and
/// the answer is 0 or a negative `errno`, which is what `mount(2)`
/// returns.
///
/// The helper sees its end of the FIFO closed when the kernel is done
/// with it; when the helper goes, `mount(2)` of those types is `ENODEV`
/// until the next one makes this call. A helper that answers nothing for
/// two minutes is killed.
///
/// Both handles go to the kernel, whatever the answer. `ACCESS_DENIED` to
/// a process that may not `mount(2)`, `ALREADY_BOUND` while another helper
/// is there, `INVALID_ARGS` for a FIFO or window a server's could not be.
pub const ECLIPSE_MOUNT_HELP: u32 = super::ECLIPSE_GATEWAY_EXT + 14;

pub(super) fn mount_help(proc: &Arc<Process>, args: &[usize; 6]) -> Result<(), ZxError> {
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::READ | Rights::WRITE);
    let window =
        proc.get_object_with_rights::<VmObject>(args[1] as u32, Rights::READ | Rights::WRITE);
    for handle in [args[0], args[1]] {
        if handle != 0 {
            let _ = proc.remove_handle(handle as u32);
        }
    }
    let (fifo, window) = (fifo?, window?);
    let helper = linux_object::fs::served::helper::register(proc.linux(), fifo.clone(), window)
        .map_err(|e| match e {
            LxError::EPERM => ZxError::ACCESS_DENIED,
            LxError::EBUSY => ZxError::ALREADY_BOUND,
            _ => ZxError::INVALID_ARGS,
        })?;
    let pid = proc.id() as usize;
    helper.killed_with(alloc::boxed::Box::new(move || {
        if let Err(e) = linux_object::process::kill_hung(pid, "mount helper") {
            warn!("mount helper: the hung helper {pid} was not killed: {e:?}");
        }
    }));
    kernel_hal::thread::spawn(async move {
        let wire: Arc<dyn KernelObject> = fifo;
        wire.wait_signal(Signal::PEER_CLOSED | linux_object::fs::served::SERVER_LEFT)
            .await;
        drop(wire);
        helper.left();
    });
    Ok(())
}
