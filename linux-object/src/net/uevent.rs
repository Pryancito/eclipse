//! `NETLINK_KOBJECT_UEVENT`: the socket device events reach userspace on.
//!
//! In Linux the kernel multicasts each uevent (`kobject_uevent_env`) on
//! group 1, as `ACTION@DEVPATH` followed by its `KEY=value` strings, and
//! udevd, once it has processed it, re-broadcasts it on group 2 behind a
//! `libudev` header. libudev monitors listen on group 2 (`"udev"`), and that
//! is how wlroots learns of a monitor plugged in: a `change` event of the
//! `drm` subsystem with `HOTPLUG=1` makes it re-read the card's connectors
//! (`backend/session/session.c`, `handle_udev_event`).
//!
//! There is no udevd here, so the kernel sends both: the kernel form on
//! group 1 and the form udevd would send on group 2, which is what a
//! process that can only run libudev, and no udevd, needs. The records are
//! the ones udevd would write for a DRM card (`DEVNAME` as an absolute path,
//! already initialized), and every message carries root's credentials, as
//! libudev requires of either group.
//!
//! Userspace cannot send on this socket: only the kernel speaks here.

use super::socket_address::*;
use crate::error::{LxError, LxResult};
use crate::fs::{FileLike, OpenFlags, PollEvents, PollStatus};
use crate::net::{Endpoint, Socket, SocketType, SysResult};
use crate::sync::{Event, EventBus};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use async_trait::async_trait;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use lock::Mutex;

// Needed by `impl_kobject!`
#[allow(unused_imports)]
use zircon_object::object::*;

/// `NETLINK_KOBJECT_UEVENT`, the protocol of `socket(AF_NETLINK, _, 15)`.
pub const NETLINK_KOBJECT_UEVENT: usize = 15;

/// The kernel's group, and the one udevd re-broadcasts on (libudev's
/// `MONITOR_GROUP_KERNEL` and `MONITOR_GROUP_UDEV`).
pub const GROUP_KERNEL: u32 = 1;
pub const GROUP_UDEV: u32 = 2;

/// Messages a socket nobody reads keeps; older ones are dropped, as a full
/// receive buffer drops them in Linux (`ENOBUFS` on the next read there).
const QUEUE_MAX: usize = 64;

/// libudev's `UDEV_MONITOR_MAGIC`, in network order on the wire.
const UDEV_MONITOR_MAGIC: u32 = 0xfeed_cafe;

/// `struct monitor_netlink_header`, which is 40 bytes.
const UDEV_HEADER_LEN: usize = 40;

static SEQNUM: AtomicU64 = AtomicU64::new(0);

struct Inner {
    /// Each message with the group it went out on.
    queue: Mutex<VecDeque<(Vec<u8>, u32)>>,
    /// The groups bound, as a mask (bit `g - 1` for group `g`).
    groups: AtomicU32,
    passcred: AtomicBool,
    bus: Arc<Mutex<EventBus>>,
}

lazy_static::lazy_static! {
    /// Every open uevent socket, for [`broadcast`].
    static ref LISTENERS: Mutex<Vec<Weak<Inner>>> = Mutex::new(Vec::new());
}

/// One device event: its action (`add`, `remove`, `change`), its sysfs path
/// and its properties, without `ACTION`, `DEVPATH` and `SEQNUM`, which
/// [`broadcast`] adds.
pub struct Uevent<'a> {
    pub action: &'a str,
    pub devpath: &'a str,
    pub props: &'a [(&'a str, &'a str)],
}

/// The `KEY=value\0` strings of `ev`, in the order Linux sends them.
fn properties(ev: &Uevent, seqnum: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut put = |k: &str, v: &str| {
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
        out.push(0);
    };
    put("ACTION", ev.action);
    put("DEVPATH", ev.devpath);
    for (k, v) in ev.props {
        put(k, v);
    }
    put("SEQNUM", &alloc::format!("{}", seqnum));
    out
}

/// The kernel form: `ACTION@DEVPATH\0` and then the properties, with
/// `DEVNAME` relative to `/dev` as the kernel sends it.
pub fn kernel_message(ev: &Uevent, seqnum: u64) -> Vec<u8> {
    let props: Vec<(&str, &str)> = ev
        .props
        .iter()
        .map(|(k, v)| match *k {
            "DEVNAME" => (*k, v.strip_prefix("/dev/").unwrap_or(v)),
            _ => (*k, *v),
        })
        .collect();
    let kernel = Uevent {
        action: ev.action,
        devpath: ev.devpath,
        props: &props,
    };
    let mut out = alloc::format!("{}@{}", ev.action, ev.devpath).into_bytes();
    out.push(0);
    out.extend(properties(&kernel, seqnum));
    out
}

/// The form udevd re-broadcasts: a `libudev` header, then the properties,
/// `DEVNAME` absolute. The filter hashes are left 0: they only feed the
/// socket filter a monitor attaches, which this socket does not run, and
/// libudev checks the subsystem again itself.
pub fn udev_message(ev: &Uevent, seqnum: u64) -> Vec<u8> {
    let props = properties(ev, seqnum);
    let mut out = Vec::with_capacity(UDEV_HEADER_LEN + props.len());
    out.extend_from_slice(b"libudev\0");
    out.extend_from_slice(&UDEV_MONITOR_MAGIC.to_be_bytes());
    for word in [
        UDEV_HEADER_LEN as u32,
        UDEV_HEADER_LEN as u32,
        props.len() as u32,
        0,
        0,
        0,
        0,
    ] {
        out.extend_from_slice(&word.to_ne_bytes());
    }
    out.extend(props);
    out
}

/// Send `ev` to every socket listening: the kernel form on group 1, the udev
/// form on group 2. Returns its `SEQNUM`.
pub fn broadcast(ev: &Uevent) -> u64 {
    let seqnum = SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
    let kernel = kernel_message(ev, seqnum);
    let udev = udev_message(ev, seqnum);
    let mut listeners = LISTENERS.lock();
    listeners.retain(|w| w.strong_count() > 0);
    for inner in listeners.iter().filter_map(Weak::upgrade) {
        let groups = inner.groups.load(Ordering::Acquire);
        for (group, msg) in [(GROUP_KERNEL, &kernel), (GROUP_UDEV, &udev)] {
            if groups & (1 << (group - 1)) == 0 {
                continue;
            }
            let mut queue = inner.queue.lock();
            if queue.len() >= QUEUE_MAX {
                queue.pop_front();
            }
            queue.push_back((msg.clone(), group));
            drop(queue);
            inner.bus.lock().set(Event::READABLE);
        }
    }
    seqnum
}

/// A `socket(AF_NETLINK, SOCK_RAW | SOCK_DGRAM, NETLINK_KOBJECT_UEVENT)`.
pub struct UeventSocket {
    base: KObjectBase,
    inner: Arc<Inner>,
    port: AtomicU32,
    flags: Mutex<OpenFlags>,
    sock_type: SocketType,
    last_msg_flags: Mutex<i32>,
}

impl UeventSocket {
    pub fn new(sock_type: SocketType) -> Self {
        let inner = Arc::new(Inner {
            queue: Mutex::new(VecDeque::new()),
            groups: AtomicU32::new(0),
            passcred: AtomicBool::new(false),
            bus: Arc::new(Mutex::new(EventBus::default())),
        });
        LISTENERS.lock().push(Arc::downgrade(&inner));
        let base = KObjectBase::new();
        let port = ((base.id % u64::from(u32::MAX)) as u32).max(1);
        Self {
            base,
            inner,
            port: AtomicU32::new(port),
            flags: Mutex::new(OpenFlags::RDWR),
            sock_type,
            last_msg_flags: Mutex::new(0),
        }
    }

    fn take(&self, data: &mut [u8], consume: bool) -> Option<(usize, u32)> {
        let mut queue = self.inner.queue.lock();
        let (msg, group) = if consume {
            queue.pop_front()?
        } else {
            queue.front().cloned()?
        };
        if queue.is_empty() {
            self.inner.bus.lock().clear(Event::READABLE);
        }
        drop(queue);
        let n = msg.len().min(data.len());
        data[..n].copy_from_slice(&msg[..n]);
        // A datagram: what does not fit is gone, and the reader is told.
        *self.last_msg_flags.lock() = if msg.len() > data.len() { 0x20 } else { 0 };
        Some((n, group))
    }

    async fn receive(&self, data: &mut [u8], consume: bool) -> (SysResult, Endpoint) {
        loop {
            if let Some((n, group)) = self.take(data, consume) {
                let from = NetlinkEndpoint::new(0, 1 << (group - 1));
                return (Ok(n), Endpoint::Netlink(from));
            }
            let none = Endpoint::Netlink(NetlinkEndpoint::new(0, 0));
            if self.flags.lock().contains(OpenFlags::NON_BLOCK) {
                return (Err(LxError::EAGAIN), none);
            }
            kernel_hal::thread::sleep_until(
                kernel_hal::timer::timer_now() + core::time::Duration::from_millis(5),
            )
            .await;
        }
    }

    fn readable(&self) -> bool {
        !self.inner.queue.lock().is_empty()
    }
}

#[async_trait]
impl Socket for UeventSocket {
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        self.receive(data, true).await
    }

    async fn peek(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        self.receive(data, false).await
    }

    /// Only the kernel sends uevents.
    fn write(&self, _data: &[u8], _to: Option<Endpoint>) -> SysResult {
        Err(LxError::EPERM)
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        (self.readable(), true, false)
    }

    async fn connect(&self, _endpoint: Endpoint) -> SysResult {
        Ok(0)
    }

    fn disconnect(&self) -> SysResult {
        Ok(0)
    }

    /// `netlink_bind`: the groups to listen on, and the port, 0 for one the
    /// kernel picks.
    fn bind(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Netlink(nl) = endpoint else {
            return Err(LxError::EINVAL);
        };
        if nl.port_id != 0 {
            self.port.store(nl.port_id, Ordering::Relaxed);
        }
        self.inner
            .groups
            .store(nl.multicast_groups_mask, Ordering::Release);
        Ok(0)
    }

    fn shutdown(&self, howto: usize) -> SysResult {
        let _ = crate::net::shutdown_sides(howto)?;
        Ok(0)
    }

    fn endpoint(&self) -> Option<Endpoint> {
        Some(Endpoint::Netlink(NetlinkEndpoint::new(
            self.port.load(Ordering::Relaxed),
            self.inner.groups.load(Ordering::Relaxed),
        )))
    }

    fn passcred(&self) -> bool {
        self.inner.passcred.load(Ordering::Relaxed)
    }

    fn set_passcred(&self, on: bool) -> SysResult {
        self.inner.passcred.store(on, Ordering::Relaxed);
        Ok(0)
    }

    /// Every message here is the kernel's: pid 0, uid 0, gid 0.
    fn recv_creds(&self) -> Option<[u8; 12]> {
        self.passcred().then_some([0; 12])
    }

    fn socket_type(&self) -> Option<SocketType> {
        Some(self.sock_type)
    }

    fn so_domain(&self) -> Option<u32> {
        Some(crate::net::Domain::AF_NETLINK as u32)
    }

    fn so_protocol(&self) -> Option<u32> {
        Some(NETLINK_KOBJECT_UEVENT as u32)
    }

    fn take_msg_flags(&self) -> i32 {
        core::mem::replace(&mut *self.last_msg_flags.lock(), 0)
    }
}

zircon_object::impl_kobject!(UeventSocket);

#[async_trait]
impl FileLike for UeventSocket {
    fn metadata(&self) -> LxResult<rcore_fs::vfs::Metadata> {
        Ok(crate::fs::anon_metadata(
            rcore_fs::vfs::FileType::Socket,
            self.id() as usize,
        ))
    }

    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        self.flags.lock().take_settable(f);
        Ok(())
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        Socket::read(self, buf).await.0
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ESPIPE)
    }

    fn write(&self, buf: &[u8]) -> LxResult<usize> {
        Socket::write(self, buf, None)
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus {
            read: self.readable(),
            write: true,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        FileLike::poll(self, events)
    }

    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_readiness_on(
            &self.inner.bus,
            mask,
            waker,
        ))
    }

    fn readiness_seq(&self, events: PollEvents) -> Option<u64> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(self.inner.bus.lock().seq_for(mask))
    }

    fn subscribe_edge(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
        seen: u64,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_edge_on(
            &self.inner.bus,
            mask,
            waker,
            seen,
        ))
    }

    fn readable_bytes(&self) -> Option<usize> {
        Some(self.inner.queue.lock().front().map_or(0, |(m, _)| m.len()))
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}

/// The devpath and properties udevd would record for DRM card `minor` of
/// the PCI function `bdf`, for a hotplug `change` event.
pub fn drm_hotplug(minor: u32, bdf: Option<(u8, u8, u8)>) -> u64 {
    let devpath = match bdf {
        Some((b, d, f)) => alloc::format!(
            "/devices/pci0000:00/0000:{:02x}:{:02x}.{:x}/drm/card{}",
            b,
            d,
            f,
            minor
        ),
        None => alloc::format!("/devices/virtual/drm/card{}", minor),
    };
    let minor_s = alloc::format!("{}", minor);
    let devname: String = alloc::format!("/dev/dri/card{}", minor);
    broadcast(&Uevent {
        action: "change",
        devpath: &devpath,
        props: &[
            ("SUBSYSTEM", "drm"),
            ("HOTPLUG", "1"),
            ("DEVNAME", &devname),
            ("DEVTYPE", "drm_minor"),
            ("MAJOR", "226"),
            ("MINOR", &minor_s),
        ],
    })
}

#[cfg(test)]
mod tests;
