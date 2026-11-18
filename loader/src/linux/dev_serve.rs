//! [`ECLIPSE_DEV_SERVE`]: a device node that a process serves, in `/dev`
//! like any other. The kernel's side of it is
//! `linux_object::fs::devfs::served`.

use alloc::sync::Arc;
use linux_object::error::LxError;
use linux_object::fs::devfs::served::{self, SERVER_LEFT};
use linux_object::process::ProcessExt;
use zircon_object::ipc::Fifo;
use zircon_object::object::{KernelObject, Rights, Signal};
use zircon_object::task::Process;
use zircon_object::vm::VmObject;
use zircon_object::{ZxError, ZxResult};

/// `eclipse_dev_serve(fifo, window, name, name_len, rdev, mode)`: put a
/// character device in `/dev` that this process serves. `fifo` is the end
/// of a FIFO of 32-byte messages the kernel keeps and `window` a VMO the
/// bytes of each request pass through, a slot of 64 KiB for each request in
/// flight, up to 16 (`linux_object::fs::devfs::served::wire` has the
/// messages). `name` is `name_len` bytes, the node's path under `/dev`
/// (`input/event4`): the directories on the way are made. `rdev` is the
/// device's major number in the high 32 bits and its minor in the low ones;
/// `mode` its permissions, 0o777 at most. Both handles go to the kernel,
/// whatever the answer.
///
/// The process answers each message on the other end of the FIFO, and
/// says there, unasked, when an open of the node becomes readable or
/// writable. When the process is gone the node goes too, and its opens
/// read as hung up; one that answers nothing for ten seconds while a
/// request waits is killed. The next server puts the node back with this
/// same call.
///
/// `ACCESS_DENIED` to a process that may not `mount(2)`, `ALREADY_EXISTS`
/// when the name is taken, `INVALID_ARGS` for a name that is not one to
/// four plain names with a `/` between each two, a major past 4095 or a
/// minor past 255, other bits in `mode`, a window that does not hold a slot
/// or a FIFO of another message size, `BAD_STATE` before `/dev` is there.
pub const ECLIPSE_DEV_SERVE: u32 = super::ECLIPSE_GATEWAY_EXT + 16;

/// The longest `name`.
const NAME_MAX: usize = 256;

pub(super) fn dev_serve(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::READ | Rights::WRITE);
    let window =
        proc.get_object_with_rights::<VmObject>(args[1] as u32, Rights::READ | Rights::WRITE);
    for handle in [args[0], args[1]] {
        if handle != 0 {
            let _ = proc.remove_handle(handle as u32);
        }
    }
    let (fifo, window) = (fifo?, window?);
    if args[3] == 0 || args[3] > NAME_MAX {
        return Err(ZxError::INVALID_ARGS);
    }
    let name = kernel_hal::user::UserInPtr::<u8>::from(args[2])
        .read_array(args[3])
        .map_err(|_| ZxError::INVALID_ARGS)?;
    let name = core::str::from_utf8(&name).map_err(|_| ZxError::INVALID_ARGS)?;
    let (major, minor) = ((args[4] as u64 >> 32) as usize, args[4] & 0xffff_ffff);
    let mode = core::convert::TryFrom::try_from(args[5]).map_err(|_| ZxError::INVALID_ARGS)?;
    let served = served::serve(proc.linux(), fifo.clone(), window, name, major, minor, mode)
        .map_err(|e| match e {
            LxError::EPERM => ZxError::ACCESS_DENIED,
            LxError::EEXIST => ZxError::ALREADY_EXISTS,
            LxError::ENODEV => ZxError::BAD_STATE,
            _ => ZxError::INVALID_ARGS,
        })?;
    let node = served.node;
    // A server that stops answering is killed, by its pid, which is its
    // own for as long as it holds the other end of the FIFO.
    let pid = proc.id() as usize;
    node.killed_with(alloc::boxed::Box::new(move || {
        let kill = linux_object::signal::Signal::SIGKILL;
        if let Err(e) = linux_object::process::send_signal_to_process(pid, kill) {
            warn!("served dev: the hung server {pid} was not killed: {e:?}");
        }
    }));
    // What the server says while nobody asks it anything -- an open became
    // readable -- and its going are read here.
    kernel_hal::thread::spawn(async move {
        let wire: Arc<dyn KernelObject> = fifo;
        loop {
            let seen = wire
                .wait_signal(Signal::READABLE | Signal::PEER_CLOSED | SERVER_LEFT)
                .await;
            if seen.contains(Signal::READABLE) && node.look() {
                continue;
            }
            // Gone, or let go of; what it said last has been read.
            if !seen.contains(Signal::READABLE) {
                node.look();
            }
            break;
        }
        drop(wire);
        node.server_left();
        served::unserve(&node);
    });
    Ok(())
}
