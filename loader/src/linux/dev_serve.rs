//! [`ECLIPSE_DEV_SERVE`]: a device node that a process serves, in `/dev`
//! like any other. The kernel's side of it is
//! `linux_object::fs::devfs::served`.

use alloc::sync::Arc;
use linux_object::error::LxError;
use linux_object::fs::devfs::served::{self, card, SERVER_LEFT};
use linux_object::process::ProcessExt;
use zircon_object::ipc::Fifo;
use zircon_object::object::{KernelObject, Rights, Signal};
use zircon_object::task::Process;
use zircon_object::vm::{VmAddressRegion, VmObject};
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
/// writable. An `mmap` of the node it answers with a VMO handle of its own
/// table, which the kernel maps in the program that asked. When the process is gone the node goes too, and its opens
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

/// `eclipse_dev_memory(fifo, reqid, addr, buf, len, flags)`: the memory of
/// the program whose `ioctl` on a node this process serves is request
/// `reqid` on `fifo` (the server's end, given to [`ECLIPSE_DEV_SERVE`]).
/// It copies `len` bytes from the program's `addr` into this process's
/// `buf`, or, with `ECLIPSE_MEMORY_TO_CLIENT` (1) in `flags`, from `buf`
/// into the program's `addr`, as `ECLIPSE_REDIRECT_MEMORY` does for a
/// redirected syscall. The answer is how many bytes were copied: fewer
/// than `len` where a page on the way is one the program could not read,
/// or write, itself.
///
/// Only while the request waits: once it is answered the program's memory
/// is out of reach again. It is for a driver whose commands hold pointers
/// to more (NVIDIA's do, to depth three), which it follows as it goes
/// instead of asking for each range in advance (`IOCTL_FETCH`).
///
/// `NOT_FOUND` for a request that is not waiting, is not an `ioctl`, or is
/// not of a node served on `fifo`; `INVALID_ARGS` for other `flags` or a
/// `buf` of this process's that cannot be read or written.
pub const ECLIPSE_DEV_MEMORY: u32 = super::ECLIPSE_GATEWAY_EXT + 20;

/// `eclipse_drm_serve(fifo, window, hello, hello_len)`: a DRM card this
/// process drives. The DRM core counts it with the others and gives it a
/// node in `/dev/dri`, and every call it makes of the card is a message on
/// `fifo`, through `window`, as with [`ECLIPSE_DEV_SERVE`] (the messages
/// and what they carry are `zcore_drivers::scheme::drm_remote`'s).
/// `hello` is `hello_len` bytes, what the driver says of the card: its
/// answer to `drm_remote::op::HELLO`. Both handles go to the kernel,
/// whatever the answer.
///
/// When the process is gone so is the card; one that answers nothing for
/// ten seconds while a call waits is killed.
///
/// `ACCESS_DENIED` to a process that may not `mount(2)`, `INVALID_ARGS`
/// for a `hello` that does not parse, a window that does not hold a slot
/// or a FIFO of another message size.
pub const ECLIPSE_DRM_SERVE: u32 = super::ECLIPSE_GATEWAY_EXT + 22;

/// The longest `hello`: a name and a few numbers.
const HELLO_MAX: usize = 4096;

pub(super) fn drm_serve(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::READ | Rights::WRITE);
    let window =
        proc.get_object_with_rights::<VmObject>(args[1] as u32, Rights::READ | Rights::WRITE);
    for handle in [args[0], args[1]] {
        if handle != 0 {
            let _ = proc.remove_handle(handle as u32);
        }
    }
    let (fifo, window) = (fifo?, window?);
    if args[3] == 0 || args[3] > HELLO_MAX {
        return Err(ZxError::INVALID_ARGS);
    }
    let hello = kernel_hal::user::UserInPtr::<u8>::from(args[2])
        .read_array(args[3])
        .map_err(|_| ZxError::INVALID_ARGS)?;
    let card = card::serve_card(
        proc.linux(),
        fifo.clone(),
        window,
        &hello,
        fence_check(proc),
    )
    .map_err(|e| match e {
        LxError::EPERM => ZxError::ACCESS_DENIED,
        _ => ZxError::INVALID_ARGS,
    })?;
    let pid = proc.id() as usize;
    card.killed_with(alloc::boxed::Box::new(move || {
        let kill = linux_object::signal::Signal::SIGKILL;
        if let Err(e) = linux_object::process::send_signal_to_process(pid, kill) {
            warn!("served drm: the hung server {pid} was not killed: {e:?}");
        }
    }));
    // Its going is read here, as a served node's is.
    kernel_hal::thread::spawn(async move {
        let wire: Arc<dyn KernelObject> = fifo;
        loop {
            let seen = wire
                .wait_signal(Signal::READABLE | Signal::PEER_CLOSED | SERVER_LEFT)
                .await;
            if seen.contains(Signal::READABLE) && card.look() {
                continue;
            }
            break;
        }
        drop(wire);
        card.gone();
    });
    Ok(())
}

/// `eclipse_nvctl_serve(fifo, window, hello, hello_len)`: the NVIDIA RM
/// this process runs, as the one `/dev/nvidiactl` and `/dev/nvidia<N>`
/// reach. The device files stay the kernel's; each escape they pass to
/// the RM is a message on `fifo`, through `window`, as with
/// [`ECLIPSE_DEV_SERVE`], and lends this process the caller's memory while
/// it waits ([`ECLIPSE_DEV_MEMORY`]), where the RM follows the pointers in
/// its parameters. `hello` is `hello_len` bytes, what the RM says of itself
/// and its GPUs (`zcore_drivers::display::nvctl::remote::hello`). Both
/// handles go to the kernel, whatever the answer.
///
/// When the process is gone so is the RM; one that answers nothing for ten
/// seconds while a call waits is killed.
///
/// `ACCESS_DENIED` to a process that may not `mount(2)`; `NOT_SUPPORTED`
/// with no `/dev/nvidiactl` (`nvidia.nvidiactl` is off, or not x86_64);
/// `ALREADY_EXISTS` while the kernel's own RM is up or another process
/// serves one; `INVALID_ARGS` for a `hello` that does not parse, a window
/// that does not hold a slot or a FIFO of another message size.
pub const ECLIPSE_NVCTL_SERVE: u32 = super::ECLIPSE_GATEWAY_EXT + 25;

#[cfg(target_arch = "x86_64")]
pub(super) fn nvctl_serve(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    use linux_object::fs::devfs::served::nvctl;
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::READ | Rights::WRITE);
    let window =
        proc.get_object_with_rights::<VmObject>(args[1] as u32, Rights::READ | Rights::WRITE);
    for handle in [args[0], args[1]] {
        if handle != 0 {
            let _ = proc.remove_handle(handle as u32);
        }
    }
    let (fifo, window) = (fifo?, window?);
    if args[3] == 0 || args[3] > HELLO_MAX {
        return Err(ZxError::INVALID_ARGS);
    }
    let hello = kernel_hal::user::UserInPtr::<u8>::from(args[2])
        .read_array(args[3])
        .map_err(|_| ZxError::INVALID_ARGS)?;
    let (whose, pinned, lender) = (proc.clone(), proc.clone(), proc.clone());
    let owns: nvctl::Owns =
        Arc::new(move |start, size| super::device_claim::owns_memory(&whose, start, size));
    let system: nvctl::SystemVmo =
        Arc::new(move |start, len| super::device_claim::pinned_slice(&pinned, start, len));
    let lend: nvctl::Lender = Arc::new(move |pages| {
        let runs = linux_object::fs::devfs::nvidia::runs_of(pages)?;
        let loan =
            super::device_claim::lend_to_devices(&lender, &runs, &pages.phys, pages.writable)?;
        Some(alloc::boxed::Box::new(loan) as alloc::boxed::Box<dyn nvctl::Loan>)
    });
    let server = nvctl::Server {
        owns,
        lend,
        id: proc.id(),
    };
    let rm = nvctl::serve_rm(proc.linux(), fifo.clone(), window, &hello, server, system).map_err(
        |e| match e {
            LxError::EPERM => ZxError::ACCESS_DENIED,
            LxError::ENODEV => ZxError::NOT_SUPPORTED,
            LxError::EEXIST => ZxError::ALREADY_EXISTS,
            _ => ZxError::INVALID_ARGS,
        },
    )?;
    let pid = proc.id() as usize;
    rm.killed_with(alloc::boxed::Box::new(move || {
        let kill = linux_object::signal::Signal::SIGKILL;
        if let Err(e) = linux_object::process::send_signal_to_process(pid, kill) {
            warn!("[nvidiactl] the hung RM server {pid} was not killed: {e:?}");
        }
    }));
    kernel_hal::thread::spawn(async move {
        let wire: Arc<dyn KernelObject> = fifo;
        loop {
            let seen = wire
                .wait_signal(Signal::READABLE | Signal::PEER_CLOSED | SERVER_LEFT)
                .await;
            if seen.contains(Signal::READABLE) && rm.look() {
                continue;
            }
            break;
        }
        drop(wire);
        rm.gone();
    });
    Ok(())
}

#[cfg(target_arch = "x86_64")]
impl linux_object::fs::devfs::served::nvctl::Loan for super::device_claim::GpuLoan {
    fn give_back(mut self: alloc::boxed::Box<Self>) {
        self.give_back_now();
    }
}

/// `eclipse_nvctl_return(loan)`: the RM this process serves is done with
/// the pages of `loan`, which [`ECLIPSE_NVCTL_SERVE`] lent it with an
/// escape: its devices lose them, and the kernel unpins them. `NOT_FOUND`
/// for a loan it does not have (given back already, or never its), and to
/// a process that serves no RM.
pub const ECLIPSE_NVCTL_RETURN: u32 = super::ECLIPSE_GATEWAY_EXT + 26;

#[cfg(target_arch = "x86_64")]
pub(super) fn nvctl_return(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    use linux_object::fs::devfs::served::nvctl;
    if nvctl::give_back(proc.id(), args[0] as u64) {
        Ok(())
    } else {
        Err(ZxError::NOT_FOUND)
    }
}

/// `eclipse_nvctl_event(os_info, flags, h_object, notify_index, info32,
/// info16)`: the RM this process serves posts an OS event to the open
/// `os_info` ([`ECLIPSE_NVCTL_SERVE`] told it of in the caller of its
/// `ALLOC_OS_EVENT`), queued there for `poll` and `RM_GET_EVENT_DATA`.
/// `flags` bit 0 says the other words are the event's data (`NvUnixEvent`);
/// with it clear they are zero, an event with no data. An open that has
/// closed takes nothing. `NOT_FOUND` to a process that serves no RM;
/// `INVALID_ARGS` for other flags, or a word too wide for its field.
pub const ECLIPSE_NVCTL_EVENT: u32 = super::ECLIPSE_GATEWAY_EXT + 27;

#[cfg(target_arch = "x86_64")]
pub(super) fn nvctl_event(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    use core::convert::TryFrom;
    use linux_object::fs::devfs::served::nvctl;
    let word = |v: usize| u32::try_from(v).map_err(|_| ZxError::INVALID_ARGS);
    let event = match args[1] {
        0 if args[2..].iter().all(|&w| w == 0) => None,
        1 => Some(nvctl::UnixEvent {
            h_object: word(args[2])?,
            notify_index: word(args[3])?,
            info32: word(args[4])?,
            info16: u16::try_from(args[5]).map_err(|_| ZxError::INVALID_ARGS)?,
        }),
        _ => return Err(ZxError::INVALID_ARGS),
    };
    if nvctl::post_event(proc.id(), args[0], event) {
        Ok(())
    } else {
        Err(ZxError::NOT_FOUND)
    }
}

/// `eclipse_boot_display(out, len)`: what the boot knows of the display
/// (the UEFI framebuffer, the monitor's EDID, `resolution=WxH`), written to
/// `out` as `zcore_drivers::display::boot_display` lays it out, for the
/// driver of a GPU this process holds to be told what the kernel's is told
/// at boot. `ACCESS_DENIED` to a process that holds no GPU;
/// `BUFFER_TOO_SMALL` for a `len` short of the layout; `INVALID_ARGS` for
/// an `out` that cannot be written.
pub const ECLIPSE_BOOT_DISPLAY: u32 = super::ECLIPSE_GATEWAY_EXT + 28;

#[cfg(target_arch = "x86_64")]
pub(super) fn boot_display(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    use linux_object::fs::devfs::served::nvctl;
    if !super::device_claim::holds_gpu(proc) {
        return Err(ZxError::ACCESS_DENIED);
    }
    if args[1] < nvctl::BOOT_DISPLAY_LEN {
        return Err(ZxError::BUFFER_TOO_SMALL);
    }
    kernel_hal::user::UserOutPtr::<u8>::from(args[0])
        .write_array(&nvctl::boot_display())
        .map_err(|_| ZxError::INVALID_ARGS)
}

/// `eclipse_driver_flags(out, len)`: the boot line's `nvidia.*` switches
/// as the kernel's NVIDIA driver was told them, written to `out` as
/// `zcore_drivers::display::driver_flags` lays them out, for the driver of
/// a GPU this process holds to be told the same. The bytes written.
/// `ACCESS_DENIED` to a process that holds no GPU; `BUFFER_TOO_SMALL` for a
/// `len` short of them; `INVALID_ARGS` for an `out` that cannot be written.
pub const ECLIPSE_DRIVER_FLAGS: u32 = super::ECLIPSE_GATEWAY_EXT + 29;

#[cfg(target_arch = "x86_64")]
pub(super) fn driver_flags(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult<usize> {
    use linux_object::fs::devfs::served::nvctl;
    if !super::device_claim::holds_gpu(proc) {
        return Err(ZxError::ACCESS_DENIED);
    }
    let flags = nvctl::driver_flags();
    if args[1] < flags.len() {
        return Err(ZxError::BUFFER_TOO_SMALL);
    }
    kernel_hal::user::UserOutPtr::<u8>::from(args[0])
        .write_array(&flags)
        .map_err(|_| ZxError::INVALID_ARGS)?;
    Ok(flags.len())
}

#[cfg(not(target_arch = "x86_64"))]
pub(super) fn driver_flags(_proc: &Arc<Process>, _args: &[usize; 6]) -> ZxResult<usize> {
    Err(ZxError::NOT_SUPPORTED)
}

#[cfg(not(target_arch = "x86_64"))]
pub(super) fn boot_display(_proc: &Arc<Process>, _args: &[usize; 6]) -> ZxResult {
    Err(ZxError::NOT_SUPPORTED)
}

#[cfg(not(target_arch = "x86_64"))]
pub(super) fn nvctl_event(_proc: &Arc<Process>, _args: &[usize; 6]) -> ZxResult {
    Err(ZxError::NOT_SUPPORTED)
}

#[cfg(not(target_arch = "x86_64"))]
pub(super) fn nvctl_return(_proc: &Arc<Process>, _args: &[usize; 6]) -> ZxResult {
    Err(ZxError::NOT_SUPPORTED)
}

#[cfg(not(target_arch = "x86_64"))]
pub(super) fn nvctl_serve(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    for handle in [args[0], args[1]] {
        if handle != 0 {
            let _ = proc.remove_handle(handle as u32);
        }
    }
    Err(ZxError::NOT_SUPPORTED)
}

/// [`ECLIPSE_DEV_MEMORY`]'s program: the address space of the one that
/// made request `args[1]` on the server's FIFO `args[0]`.
pub(super) fn asker(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult<Arc<VmAddressRegion>> {
    if args[5] & !1 != 0 {
        return Err(ZxError::INVALID_ARGS);
    }
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::WRITE)?;
    let reqid: u32 = core::convert::TryFrom::try_from(args[1]).map_err(|_| ZxError::NOT_FOUND)?;
    served::asker_of(fifo.as_ref(), reqid).ok_or(ZxError::NOT_FOUND)
}

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
    // Its answers to an `mmap` name a VMO by a handle in its own table,
    // looked up there while it lives: one it may map and read, and write
    // only when the handle says so.
    let server = Arc::downgrade(proc);
    node.vmos_from(alloc::boxed::Box::new(move |handle| {
        let server = server.upgrade()?;
        let handle: u32 = core::convert::TryFrom::try_from(handle).ok()?;
        let (vmo, rights) = server.get_object_and_rights::<VmObject>(handle).ok()?;
        rights
            .contains(Rights::MAP | Rights::READ)
            .then(|| (vmo, rights.contains(Rights::WRITE)))
    }));
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

/// Where the kernel reads a fence of the card `proc` serves: memory it
/// pinned, never anyone else's (`syncobj_serve`).
fn fence_check(proc: &Arc<Process>) -> card::FenceCheck {
    let proc = Arc::downgrade(proc);
    Arc::new(move |f| {
        let proc = proc.upgrade()?;
        super::syncobj_serve::served_fence(&proc, f.phys, f.ctx)
    })
}
