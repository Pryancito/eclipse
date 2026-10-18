//! The OS events of `/dev/nvidiactl` and `/dev/nvidia<N>`: how the RM tells
//! a user-mode client that something it asked to hear about has happened.
//!
//! On Linux a client opens a fresh file for each event it wants and
//! registers it with `NV_ESC_ALLOC_OS_EVENT`, issued on that same file and
//! naming its own fd number for it. It then allocates an
//! `NV01_EVENT_OS_EVENT` object in the RM whose `data` is that fd number.
//! The RM trades the number for the registration (`osUserHandleToKernelPtr`)
//! and keeps it; when the event fires, the RM posts to it (`postEvent`,
//! os.c), and `nv_post_event` (nv.c) queues an `NvUnixEvent` on the file and
//! wakes whoever polls it. The client reads the queue back with
//! `NV_ESC_RM_GET_EVENT_DATA`.
//!
//! This is the registration half: osapi.c's `allocate_os_event`,
//! `free_os_event` and `free_os_events`, and os.c's
//! `osUserHandleToKernelPtr`, `osDereferenceObjectCount`, `postEvent` and
//! `osSetEvent`. The walk over an object's notifications, which decides
//! which registrations an RM event reaches, is the RM's own C
//! (`osNotifyEvent` and the `osEventNotification*` family in
//! eclipse_rm_frontend.c). The queue on the file is the kernel's, reached
//! through [`set_event_sink`].
//!
//! One departure: what the RM keeps for a registration is not a pointer to
//! it but its id, which is only ever looked up here. One the RM holds after
//! the registration is gone finds nothing, so posting to it or dropping it
//! does nothing, where Linux trusts the pointer.
//!
//! The registrations have a lock of their own (Linux's `event_spinlock`),
//! taken by the escapes and by the RM's callbacks, and never held across a
//! call out of this file.
// The callbacks keep the names and parameter names the RM declares them with.
#![allow(non_snake_case)]
use crate::types::*;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lock::Mutex;

/// `NV_ERR_INVALID_EVENT` (nvstatuscodes.h).
pub const NV_ERR_INVALID_EVENT: NV_STATUS = 0x0000_0028;
/// `NV_ERR_OBJECT_NOT_FOUND` (nvstatuscodes.h).
pub const NV_ERR_OBJECT_NOT_FOUND: NV_STATUS = 0x0000_0057;

/// The most registrations held at once, over every open. Linux has no limit
/// but the kernel heap, and the fd a client names is a number it chooses,
/// never checked against its fd table; past this, ALLOC_OS_EVENT answers
/// what Linux answers for any failure to register,
/// `NV_ERR_INSUFFICIENT_RESOURCES`. A CUDA process registers a few dozen.
pub const MAX_REGISTRATIONS: usize = 4096;

/// `NvUnixEvent` (nvos.h): one event as RM_GET_EVENT_DATA hands it to the
/// client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnixEvent {
    /// The RM event object that fired (`hObject`).
    pub h_object: u32,
    /// Which of the object's notifiers (`NotifyIndex`).
    pub notify_index: u32,
    pub info32: u32,
    pub info16: u16,
}

impl UnixEvent {
    /// `sizeof(NvUnixEvent)`: four fields of 4, 4, 4 and 2 bytes, padded.
    pub const SIZE: usize = 16;

    /// The bytes `get_os_event_data` copies out, padding zeroed as its
    /// `os_mem_set` leaves it.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.h_object.to_le_bytes());
        b[4..8].copy_from_slice(&self.notify_index.to_le_bytes());
        b[8..12].copy_from_slice(&self.info32.to_le_bytes());
        b[12..14].copy_from_slice(&self.info16.to_le_bytes());
        b
    }
}

/// One registration: `nv_event_t`, less the fields `nv_post_event` fills in
/// per event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Registration {
    /// What the RM keeps in place of a pointer to this. Never 0.
    id: u64,
    /// The open the events are queued on (`nvfp`): the one the
    /// ALLOC_OS_EVENT was issued on.
    os_info: usize,
    /// The client it was registered for (`hParent`).
    client: u32,
    /// The client's fd number for that open, which is how the RM names it.
    fd: u32,
    /// Cleared when the client frees it, or its client or its open goes:
    /// nothing more is posted to it, nothing finds it by its fd, and it is
    /// dropped once the RM holds no reference to it.
    active: bool,
    /// References the RM holds (`refcount`): one for each RM event object
    /// bound to it, and one for each post in flight.
    refcount: u32,
}

/// Every registration, active or still referenced (`nv_ctl_state`'s
/// `event_list`). Linux unlinks one when it stops being active and frees it
/// when the last reference goes; here it stays listed until then, so an id
/// the RM hands back always finds it.
struct Registrations {
    list: Vec<Registration>,
    next_id: u64,
}

impl Registrations {
    const fn new() -> Self {
        Registrations {
            list: Vec::new(),
            next_id: 1,
        }
    }

    fn active_by_fd(&self, client: u32, fd: u32) -> Option<usize> {
        self.list
            .iter()
            .position(|r| r.active && r.client == client && r.fd == fd)
    }

    fn by_id(&self, id: u64) -> Option<usize> {
        self.list.iter().position(|r| r.id == id)
    }

    /// `allocate_os_event`: register `fd` for `client` on the open
    /// `os_info`. One registration per (client, fd); every failure answers
    /// `NV_ERR_INSUFFICIENT_RESOURCES`, as Linux maps them.
    fn alloc(&mut self, client: u32, os_info: usize, fd: u32) -> NV_STATUS {
        if self.active_by_fd(client, fd).is_some() || self.list.len() >= MAX_REGISTRATIONS {
            return NV_ERR_INSUFFICIENT_RESOURCES;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.list.push(Registration {
            id,
            os_info,
            client,
            fd,
            active: true,
            refcount: 0,
        });
        NV_OK
    }

    /// `free_os_event_under_lock`: no longer active, and gone unless the RM
    /// still holds it.
    fn retire(&mut self, at: usize) {
        self.list[at].active = false;
        if self.list[at].refcount == 0 {
            self.list.swap_remove(at);
        }
    }

    /// `free_os_event`: the client's registration of `fd`.
    fn free(&mut self, client: u32, fd: u32) -> NV_STATUS {
        match self.active_by_fd(client, fd) {
            Some(at) => {
                self.retire(at);
                NV_OK
            }
            None => NV_ERR_INVALID_EVENT,
        }
    }

    /// `free_os_events` for the close of the open `os_info`
    /// (`RmFreeUnusedClients`).
    ///
    /// Linux matches either key in one walk, so its close (client 0) also
    /// takes any registration made for client 0, whichever open made it.
    /// Here each walk matches its own key only.
    fn free_open(&mut self, os_info: usize) {
        self.retire_where(|r| r.os_info == os_info);
    }

    /// `free_os_events` for the free of `client` (`rm_client_free_os_events`).
    fn free_client(&mut self, client: u32) {
        self.retire_where(|r| r.client == client);
    }

    fn retire_where(&mut self, matches: impl Fn(&Registration) -> bool) {
        // Backwards: what `retire` moves into a freed slot comes from the
        // end, which has been looked at already.
        for at in (0..self.list.len()).rev() {
            if self.list[at].active && matches(&self.list[at]) {
                self.retire(at);
            }
        }
    }

    /// `osUserHandleToKernelPtr`: the id of the client's registration of
    /// `fd`, referenced for the RM event object about to hold it.
    fn reference_by_fd(&mut self, client: u32, fd: u32) -> Option<u64> {
        let at = self.active_by_fd(client, fd)?;
        self.list[at].refcount += 1;
        Some(self.list[at].id)
    }

    /// `referenceEventForPost`: the open to queue on, with the registration
    /// held until [`unreference`](Self::unreference). `None` for one that is
    /// not active, or not there.
    fn reference_for_post(&mut self, id: u64) -> Option<usize> {
        let at = self.by_id(id).filter(|&at| self.list[at].active)?;
        self.list[at].refcount += 1;
        Some(self.list[at].os_info)
    }

    /// `osDereferenceObjectCount` and `dereferenceEventForPost`: one
    /// reference fewer, and the registration goes with its last one once it
    /// is no longer active. Whether `id` named one.
    fn unreference(&mut self, id: u64) -> bool {
        let Some(at) = self.by_id(id) else {
            return false;
        };
        let r = &mut self.list[at];
        r.refcount = r.refcount.saturating_sub(1);
        if r.refcount == 0 && !r.active {
            self.list.swap_remove(at);
        }
        true
    }

    /// How many registrations are listed, active or still referenced.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.list.len()
    }
}

/// Every open's registrations.
static REGISTRATIONS: Mutex<Registrations> = Mutex::new(Registrations::new());

/// `fn(os_info, event)`, registered by the kernel: queue `event` on the open
/// `os_info` names (`nv_post_event`), `None` being an event without data. 0
/// = none yet, and then nothing is ever posted.
static SINK: AtomicUsize = AtomicUsize::new(0);

/// Register the kernel's queue for posted events. `nvidia-rm-sys` cannot
/// reach the open files (it sits below the kernel), so posts go through
/// this.
pub fn set_event_sink(f: fn(usize, Option<UnixEvent>)) {
    SINK.store(f as usize, Ordering::Release);
}

fn sink() -> Option<fn(usize, Option<UnixEvent>)> {
    let f = SINK.load(Ordering::Acquire);
    // SAFETY: only ever stored from `set_event_sink`.
    (f != 0).then(|| unsafe { core::mem::transmute::<usize, fn(usize, Option<UnixEvent>)>(f) })
}

/// NV_ESC_ALLOC_OS_EVENT, issued on the open `os_info`.
pub fn alloc_os_event(client: u32, os_info: usize, fd: u32) -> NV_STATUS {
    REGISTRATIONS.lock().alloc(client, os_info, fd)
}

/// NV_ESC_FREE_OS_EVENT. Like Linux, it frees the client's registration of
/// `fd` whichever open it was made on.
pub fn free_os_event(client: u32, fd: u32) -> NV_STATUS {
    REGISTRATIONS.lock().free(client, fd)
}

/// The close of the open `os_info`: none of its registrations takes another
/// event. Run by [`release`](super::release), after the open's clients.
pub(crate) fn free_open_events(os_info: usize) {
    REGISTRATIONS.lock().free_open(os_info);
}

/// `postEvent`: hand one event to the open behind registration `id`, if it
/// is still active.
fn post(id: u64, event: Option<UnixEvent>) {
    let Some(os_info) = REGISTRATIONS.lock().reference_for_post(id) else {
        return;
    };
    if let Some(sink) = sink() {
        sink(os_info, event);
    }
    REGISTRATIONS.lock().unreference(id);
}

/// `rm_client_free_os_events`: what escape.c runs once an RM_FREE has freed
/// a whole client. Called from eclipse_rm_frontend.c's RM_FREE.
#[no_mangle]
pub extern "C" fn rm_client_free_os_events(client: NvU32) {
    REGISTRATIONS.lock().free_client(client);
}

/// `osUserHandleToKernelPtr` (os.c): the RM is allocating an
/// `NV01_EVENT_OS_EVENT` (or binding an event buffer, or a semaphore
/// surface listener) for a user client, whose `data` is the fd number it
/// registered. `*pHandle` becomes the registration's id, referenced until
/// the RM drops it with `osDereferenceObjectCount`.
#[no_mangle]
pub extern "C" fn osUserHandleToKernelPtr(
    hClient: NvU32,
    Handle: *mut c_void,
    pHandle: *mut *mut c_void,
) -> NV_STATUS {
    if pHandle.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    // `NvU32 fd = (NvU64)hEvent`: the fd number, truncated as Linux does.
    let fd = Handle as usize as u32;
    match REGISTRATIONS.lock().reference_by_fd(hClient, fd) {
        Some(id) => {
            // SAFETY: the RM's own out-parameter, checked non-null above.
            unsafe { *pHandle = id as usize as *mut c_void };
            NV_OK
        }
        None => NV_ERR_OBJECT_NOT_FOUND,
    }
}

/// `osDereferenceObjectCount` (os.c): the RM event object holding
/// registration `pEvent` is gone. An id that names no registration answers
/// `NV_ERR_INVALID_EVENT`, where Linux would follow the pointer.
#[no_mangle]
pub extern "C" fn osDereferenceObjectCount(pEvent: *mut c_void) -> NV_STATUS {
    if REGISTRATIONS.lock().unreference(pEvent as usize as u64) {
        NV_OK
    } else {
        NV_ERR_INVALID_EVENT
    }
}

/// `osSetEvent` (os.c): an event without data on registration `eventID`,
/// which the fabric and IMEX code fire. Answers 1, as Linux's always does.
#[no_mangle]
pub extern "C" fn osSetEvent(pGpu: *mut c_void, eventID: *mut c_void) -> NvU32 {
    let _ = pGpu;
    post(eventID as usize as u64, None);
    1
}

/// One OS event the RM's notification walk (eclipse_rm_frontend.c) found to
/// fire: `postEvent(event, hEvent, notifyIndex, info32, info16,
/// bDataValid)`.
#[no_mangle]
pub extern "C" fn eclipse_rm_frontend_post_event(
    event: *mut c_void,
    hEvent: NvU32,
    notifyIndex: NvU32,
    info32: NvU32,
    info16: NvU16,
    bDataValid: NvU32,
) {
    let data = (bDataValid != 0).then_some(UnixEvent {
        h_object: hEvent,
        notify_index: notifyIndex,
        info32,
        info16,
    });
    post(event as usize as u64, data);
}

#[cfg(test)]
mod tests;
