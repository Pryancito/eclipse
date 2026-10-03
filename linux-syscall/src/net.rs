use super::*;
use crate::file::{anon_fd_flags, ANON_CLOEXEC, ANON_NONBLOCK};
use crate::outparams::hand_out_pair;
use alloc::vec::Vec;
use core::convert::TryInto;
use core::mem::size_of;
use kernel_hal::user::UserInOutPtr;
use linux_object::{
    fs::{split_path, FileLike, OpenFlags, PollEvents},
    net::*,
    thread::ThreadExt,
};

const MSG_DONTWAIT: usize = 0x40;
const MSG_PEEK: usize = 0x2;
const MSG_NOSIGNAL: usize = 0x4000;
/// Close-on-exec for fds installed from `SCM_RIGHTS` (`linux/socket.h`).
const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;

/// Whether `recvmsg`/`recvmmsg` should mark SCM_RIGHTS fds `FD_CLOEXEC`.
fn scm_rights_cloexec(flags: usize) -> bool {
    flags & MSG_CMSG_CLOEXEC != 0
}

/// A socket whose `write` answers `EAGAIN` for "queue full": unix (bounded
/// peer buffer) and UDP (smoltcp's transmit ring). TCP waits on its own.
pub(crate) fn queue_is_bounded(file_like: &Arc<dyn FileLike>) -> bool {
    file_like.downcast_ref::<UnixSocketState>().is_some()
        || file_like.downcast_ref::<UdpSocketState>().is_some()
}

/// How a `send`-family call treats the two things its flags decide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SendMode {
    /// Wait for room in a full queue instead of answering `EAGAIN`.
    wait: bool,
    /// Pair `EPIPE` with `SIGPIPE`.
    sigpipe: bool,
}

/// `sock_sendmsg` flag handling. Waiting is for a socket whose `write`
/// queues into a bounded buffer and says `EAGAIN` when it is full (unix and
/// UDP) unless the fd is `O_NONBLOCK` or the call says `MSG_DONTWAIT`; a
/// TCP socket's `write` waits for window on its own. `SIGPIPE` is the
/// default for a dead peer, and `MSG_NOSIGNAL` is the one way to opt out.
///
/// Both flags used to be ignored: a blocking `sendto`/`sendmsg` on a full
/// unix socket came back `EAGAIN`, and `EPIPE` never killed anyone.
fn send_mode(flags: usize, fd_non_block: bool, bounded_queue: bool) -> SendMode {
    SendMode {
        wait: bounded_queue && !fd_non_block && flags & MSG_DONTWAIT == 0,
        sigpipe: flags & MSG_NOSIGNAL == 0,
    }
}

/// `struct mmsghdr` from `<sys/socket.h>`: one batch entry for
/// `sendmmsg`/`recvmmsg` — a plain `msghdr` plus the per-message transfer
/// count the kernel writes back. Only its layout is used (stride and the
/// `msg_len` offset); the per-entry `msg_hdr` is re-read by the wrapped
/// single-message syscalls straight from user memory.
#[repr(C)]
#[allow(dead_code)]
struct MMsgHdr {
    msg_hdr: MsgHdr,
    msg_len: u32,
    _pad: u32,
}

/// Read a `sockaddr` from user space, honoring the user-supplied `addrlen`.
///
/// `SockAddr` is a union whose alignment (4, coming from the `u32` fields of
/// the IP/netlink variants) is stricter than a C `struct sockaddr_un` (only
/// 2-byte aligned), and whose size (~110 B) is larger than most concrete
/// address structs. Reading it directly with `UserInPtr::<SockAddr>::read()`
/// therefore had two bugs:
///   (a) it rejected perfectly valid 2-byte-aligned `sockaddr_un` pointers with
///       `EFAULT` (the alignment `check()` requires 4-byte alignment) — this is
///       exactly why connecting to the X11 unix socket failed with
///       "unable to connect to X server: Bad address"; and
///   (b) it always read the full union size regardless of `addrlen`, over-
///       reading past a short user buffer that sits near the end of a mapping.
///
/// Copy exactly `addrlen` bytes (capped at the union size) byte-wise, at
/// 1-byte alignment, into a zeroed buffer instead, matching Linux
/// `move_addr_to_kernel` semantics.
#[allow(unsafe_code)]
fn read_sockaddr(addr: usize, addrlen: usize) -> Result<SockAddr, LxError> {
    if addr == 0 {
        return Err(LxError::EFAULT);
    }
    let n = addrlen.min(size_of::<SockAddr>());
    // Zeroed so any address bytes the user did not supply (`addrlen` shorter
    // than the concrete struct) read back as zero, as Linux does.
    let mut storage: SockAddr = unsafe { core::mem::zeroed() };
    if n > 0 {
        let bytes: UserInPtr<u8> = addr.into();
        let src = bytes.read_array(n)?;
        unsafe {
            core::ptr::copy_nonoverlapping(
                src.as_ptr(),
                &mut storage as *mut SockAddr as *mut u8,
                n,
            );
        }
    }
    Ok(storage)
}

/// Copy an option value out to a `getsockopt` caller.
///
/// `optlen` is a value-result argument (like Linux's `optlen`): the kernel must
/// write at most the caller-supplied `*optlen` bytes and then store the true
/// size back. Previously `optlen` was write-only and the input size was ignored,
/// so an option larger than the caller's buffer (e.g. the 12-byte SO_PEERCRED
/// `ucred` written into a 4-byte buffer) overflowed adjacent user memory.
/// `struct ucred` for `SO_PEERCRED`: `pid`, then the EFFECTIVE uid and gid of
/// the process it names (`cred_to_ucred`, which reads `euid`/`egid`). When
/// no such process exists any more the ids are `-1`, which is what Linux
/// reports for a peer whose credentials it does not hold (`overflowuid`).
fn ucred_of(pid: i32) -> [u8; 12] {
    let (uid, gid) = linux_object::process::find_process(pid as u64)
        .and_then(|p| p.try_linux().map(|lp| lp.credentials()))
        .map(|c| (c.euid, c.egid))
        .unwrap_or((u32::MAX, u32::MAX));
    let mut bytes = [0u8; 12];
    for (i, w) in [pid as u32, uid, gid].iter().enumerate() {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
    }
    bytes
}

fn write_sockopt_out(
    optval: UserOutPtr<u32>,
    mut optlen: UserInOutPtr<u32>,
    value: &[u8],
) -> SysResult {
    let max = optlen.read()? as usize;
    let n = sockopt_out_len(max, value.len());
    if n > 0 {
        let mut dst: UserOutPtr<u8> = optval.as_addr().into();
        dst.write_array(&value[..n])?;
    }
    // Report the number of bytes actually written, not the option's full size.
    // `sock_getsockopt()` clamps (`if (len > lv) len = lv;`) before its
    // `put_user(len, optlen)`, and a caller that believes an `optlen` larger
    // than the buffer it supplied goes on to read bytes the kernel never
    // wrote — its own uninitialised stack.
    optlen.write(n as u32)?;
    Ok(0)
}

/// How many bytes of an option value `getsockopt` copies out, which is also
/// what it reports back through `optlen`.
///
/// `sock_getsockopt()` clamps (`if (len > lv) len = lv;`) before its
/// `put_user(len, optlen)`. Reporting the option's *full* size instead, which
/// is what this used to do, tells a caller with a smaller buffer that the
/// kernel wrote more than it did: the caller then reads its own uninitialised
/// stack as part of the answer. `SO_PEERCRED` into a 4-byte `int` is the case
/// that turns up, since a `struct ucred` is 12.
fn sockopt_out_len(requested: usize, actual: usize) -> usize {
    actual.min(requested)
}

/// `struct cmsghdr` is `{ size_t cmsg_len; int cmsg_level; int cmsg_type; }`:
/// 16 bytes on a 64-bit target, followed by the payload, with each message
/// `CMSG_ALIGN`ed to 8.
const CMSG_HDR_LEN: usize = 16;
/// `SOL_SOCKET`, as it appears in `cmsg_level`.
const SOL_SOCKET_LEVEL: i32 = 1;
/// `SCM_RIGHTS`, as it appears in `cmsg_type`.
const SCM_RIGHTS: i32 = 1;
/// `SCM_CREDENTIALS`, as it appears in `cmsg_type`: a `struct ucred` the
/// KERNEL fills in, which is what makes it worth anything to the receiver.
const SCM_CREDENTIALS: i32 = 2;
/// `msg_flags`: ancillary data was dropped because the caller's control buffer
/// could not hold it.
const MSG_CTRUNC: i32 = 0x8;
/// `SOL_SOCKET`, as `setsockopt`/`getsockopt` take it in `level`.
const SOL_SOCKET: usize = 1;
/// `SO_PASSCRED`: attach the sender's credentials to every message read from
/// this socket.
const SO_PASSCRED: usize = 16;
/// Most file descriptors one `sendmsg` may carry (`SCM_MAX_FD`,
/// include/net/scm.h). Linux answers `EINVAL` above it; without a cap, one
/// `sendmsg` with a 64 KiB control buffer asks the receiver to install 16000
/// descriptors.
const SCM_MAX_FD: usize = 253;

/// Walk a control buffer and return the file descriptor numbers its
/// `SCM_RIGHTS` messages carry, in order.
///
/// Everything here comes from userspace, including `cmsg_len`, so every step
/// is checked: a length that wraps past the end of the buffer, one shorter
/// than the header it claims to be, and an aligned step that would not move
/// forward all stop the walk instead of slicing out of range or spinning.
/// A malformed *tail* is simply where the walk ends, which is what
/// `__cmsg_nxthdr()` does; only the fd count is an error, because Linux makes
/// it one.
fn parse_scm_rights_fds(ctrl: &[u8]) -> Result<Vec<i32>, LxError> {
    let mut fds: Vec<i32> = Vec::new();
    let mut off = 0usize;
    while off + CMSG_HDR_LEN <= ctrl.len() {
        let cmsg_len = u64::from_ne_bytes(ctrl[off..off + 8].try_into().unwrap()) as usize;
        let level = i32::from_ne_bytes(ctrl[off + 8..off + 12].try_into().unwrap());
        let typ = i32::from_ne_bytes(ctrl[off + 12..off + 16].try_into().unwrap());
        let cmsg_end = match off.checked_add(cmsg_len) {
            Some(end) if cmsg_len >= CMSG_HDR_LEN && end <= ctrl.len() => end,
            _ => break,
        };
        if level == SOL_SOCKET_LEVEL && typ == SCM_RIGHTS {
            // A trailing partial fd is not one: `as_chunks` drops it, as the
            // kernel's `(cmsg_len - sizeof(cmsghdr)) / sizeof(int)` does.
            for chunk in ctrl[off + CMSG_HDR_LEN..cmsg_end].as_chunks::<4>().0 {
                if fds.len() == SCM_MAX_FD {
                    return Err(LxError::EINVAL);
                }
                fds.push(i32::from_ne_bytes(*chunk));
            }
        }
        // CMSG_ALIGN(cmsg_len); checked, and required to move forward so a
        // wrapped or zero step cannot spin forever.
        let step = match cmsg_len.checked_add(7).map(|v| v & !7) {
            Some(s) if s > 0 => s,
            _ => break,
        };
        off = match off.checked_add(step) {
            Some(n) => n,
            None => break,
        };
    }
    Ok(fds)
}

/// Build the single `SCM_RIGHTS` control message `recvmsg` hands back for
/// `fds`, in the layout [`parse_scm_rights_fds`] reads.
///
/// The two are the same 16-byte header written twice, once in each direction,
/// which is why the tests below round-trip them against each other rather than
/// each against a hand-written blob.
fn build_scm_rights_cmsg(fds: &[i32]) -> Vec<u8> {
    build_cmsg(
        SCM_RIGHTS,
        &fds.iter()
            .flat_map(|f| f.to_ne_bytes())
            .collect::<Vec<u8>>(),
    )
}

/// Build one `SOL_SOCKET` control message around `payload`.
///
/// `cmsg_len` counts the header and the payload and NOT the padding, exactly
/// as `CMSG_LEN` does; the padding that follows is what `CMSG_NXTHDR` steps
/// over to reach the next message. Writing the padded length into the header
/// instead would make a lone message look longer than it is and a reader
/// walking two of them land past the second.
fn build_cmsg(typ: i32, payload: &[u8]) -> Vec<u8> {
    let cmsg_len = CMSG_HDR_LEN + payload.len();
    let mut buf = Vec::with_capacity(cmsg_align(cmsg_len));
    buf.extend_from_slice(&(cmsg_len as u64).to_ne_bytes());
    buf.extend_from_slice(&SOL_SOCKET_LEVEL.to_ne_bytes());
    buf.extend_from_slice(&typ.to_ne_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// `CMSG_ALIGN`: control messages sit on 8-byte boundaries.
fn cmsg_align(len: usize) -> usize {
    (len + 7) & !7
}

/// The control buffer `recvmsg` hands back: the descriptors the peer attached,
/// then -- when `SO_PASSCRED` is on -- the sender's credentials.
///
/// The second message only exists because the first may be short: a message of
/// 20 bytes is followed by 4 bytes of padding before the next header, and a
/// reader that walked without it would read the credentials out of the middle
/// of the descriptors.
fn build_recv_cmsgs(fds: &[i32], creds: Option<&[u8; 12]>) -> Vec<u8> {
    let mut buf = Vec::new();
    if !fds.is_empty() {
        buf.extend(build_scm_rights_cmsg(fds));
    }
    if let Some(creds) = creds {
        if !buf.is_empty() {
            buf.resize(cmsg_align(buf.len()), 0);
        }
        buf.extend(build_cmsg(SCM_CREDENTIALS, creds));
    }
    buf
}

impl Syscall<'_> {
    /// creates an endpoint for communication and returns a file descriptor that refers to that endpoint.
    pub fn sys_socket(&mut self, domain: usize, _type: usize, protocol: usize) -> SysResult {
        info!(
            "sys_socket: domain:{}, type:{}, protocol:{}",
            domain, _type, protocol
        );
        let domain = match Domain::try_from(domain) {
            Ok(domain) => domain,
            Err(_) => {
                warn!("sys_socket: invalid domain: {}", domain);
                return Err(LxError::EAFNOSUPPORT);
            }
        };
        let socket_type_val = _type & SOCKET_TYPE_MASK;
        let socket_type = match SocketType::try_from(socket_type_val) {
            Ok(t) => t,
            Err(_) => {
                warn!(
                    "sys_socket: invalid socket type: {:#x} (masked: {:#x})",
                    _type, socket_type_val
                );
                return Err(LxError::EINVAL);
            }
        };
        // Same trap as the old `pipe2` / `eventfd2` path: the high bits of
        // `type` are only SOCK_CLOEXEC | SOCK_NONBLOCK. Truncating let a
        // stray bit succeed, and worse, a bit that coincides with some other
        // `OpenFlags` name (APPEND, …) was applied to the new socket.
        let flags = anon_fd_flags(_type & !SOCKET_TYPE_MASK, ANON_CLOEXEC | ANON_NONBLOCK)?;
        let protocol_num = protocol;
        let protocol = Protocol::try_from(protocol_num).ok();

        info!(
            "sys_socket: domain:{:?}, type:{:?}, protocol:{:?}",
            domain, socket_type, protocol
        );

        let socket: Arc<dyn FileLike> = match (domain, socket_type, protocol) {
            (Domain::AF_INET, SocketType::SOCK_STREAM, Some(Protocol::IPPROTO_IP))
            | (Domain::AF_INET, SocketType::SOCK_STREAM, Some(Protocol::IPPROTO_TCP)) => {
                Arc::new(TcpSocketState::new(false)?)
            }
            (Domain::AF_INET6, SocketType::SOCK_STREAM, Some(Protocol::IPPROTO_IP))
            | (Domain::AF_INET6, SocketType::SOCK_STREAM, Some(Protocol::IPPROTO_TCP)) => {
                Arc::new(TcpSocketState::new(true)?)
            }
            (Domain::AF_INET, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_IP))
            | (Domain::AF_INET, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_UDP)) => {
                Arc::new(UdpSocketState::new(false)?)
            }
            (Domain::AF_INET6, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_IP))
            | (Domain::AF_INET6, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_UDP)) => {
                Arc::new(UdpSocketState::new(true)?)
            }
            // Linux ping(8) uses SOCK_DGRAM + IPPROTO_ICMP (ping_socket).
            (Domain::AF_INET, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_ICMP)) => {
                Arc::new(IcmpSocketState::new(false)?)
            }
            (Domain::AF_INET6, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_ICMPV6)) => {
                Arc::new(IcmpSocketState::new(true)?)
            }
            // AF_INET/AF_INET6 raw sockets (some userlands probe these)
            (Domain::AF_INET, SocketType::SOCK_RAW, _) => {
                Arc::new(RawSocketState::new((protocol_num & 0xff) as u8, false)?)
            }
            (Domain::AF_INET6, SocketType::SOCK_RAW, _) => {
                Arc::new(RawSocketState::new((protocol_num & 0xff) as u8, true)?)
            }
            // AF_NETLINK sockets for interface/address discovery (iproute-style)
            (Domain::AF_NETLINK, SocketType::SOCK_RAW, _)
            | (Domain::AF_NETLINK, SocketType::SOCK_DGRAM, _) => {
                Arc::new(NetlinkSocketState::new(socket_type))
            }
            // AF_PACKET sockets (used by udhcpc for raw ethernet operations)
            (Domain::AF_PACKET, SocketType::SOCK_RAW, _)
            | (Domain::AF_PACKET, SocketType::SOCK_DGRAM, _) => {
                PacketSocketState::new(socket_type, u16::from_be(protocol_num as u16))?
            }
            // AF_UNIX sockets
            (Domain::AF_UNIX, _, _) => {
                // Same gate as `socketpair`: only STREAM/DGRAM/SEQPACKET.
                // This arm used to take every `SocketType` and implement them
                // all as a byte stream, so `socket(AF_UNIX, SOCK_RDM, 0)`
                // succeeded where Linux says `ESOCKTNOSUPPORT`.
                let socket_type = unix_socket_type(socket_type)?;
                unix_protocol(protocol_num)?;
                let s = UnixSocketState::new();
                // Record our PID so a peer (e.g. seatd) can read it via
                // SO_PEERCRED when it accepts our connection.
                s.set_owner_pid(self.zircon_process().id() as i32);
                // Record the request: `sendmsg` must not truncate an oversized
                // message on a socket the user created as a datagram or
                // seqpacket.
                s.set_socket_type(socket_type);
                s
            }
            (_, _, _) => {
                // A domain/type/protocol combo this kernel does not wire up
                // is `EPROTONOSUPPORT`, not `ENOSYS`. `ENOSYS` told callers
                // the *syscall* was missing (glibc then disables whole
                // families); Linux and busybox probe with this and expect
                // a protocol errno so they can fall back cleanly.
                info!(
                    "sys_socket: unsupported socket type: domain={:?}, type={:?}, protocol={:?}",
                    domain, socket_type, protocol_num
                );
                return Err(LxError::EPROTONOSUPPORT);
            }
        };

        socket.set_flags(flags)?;
        let fd = self.linux_process().add_socket(socket)?; // dyn FileLike
        Ok(fd.into())
    }

    ///  connects the socket referred to by the file descriptor sockfd to the address specified by addr.
    pub async fn sys_connect(
        &mut self,
        sockfd: usize,
        addr: UserInPtr<SockAddr>,
        addrlen: usize,
    ) -> SysResult {
        info!(
            "sys_connect: sockfd:{}, addr:{:?}, addrlen:{}",
            sockfd, addr, addrlen
        );
        let endpoint = sockaddr_to_endpoint(read_sockaddr(addr.as_addr(), addrlen)?, addrlen)?;
        let proc = self.linux_process();
        let file_like = proc.get_file_like(sockfd.into())?;

        if let Endpoint::Unix(path) = &endpoint {
            if let Ok(client) = file_like.clone().downcast_arc::<UnixSocketState>() {
                // `unix_stream_connect`: already connected (or a listener)
                // is `EISCONN`. Without the check a second `connect` rewired
                // the peer and queued another accept silently.
                client.may_connect()?;
                // ENOENT / ECONNREFUSED exactly as `UnixSocketState::
                // resolve_listener` decides (pathname vs abstract, bound
                // but not listening): this fast path is the one every
                // AF_UNIX connect(2) takes, so the rule must live in the
                // shared helper, not be re-derived here.
                let server = UnixSocketState::resolve_listener(path)?;
                // Establish the connection now: create the server's end,
                // wire it to the client, and queue it for accept(). Wiring
                // at connect time (rather than in accept) lets the client
                // send its first bytes — e.g. the X11 handshake — before
                // the server has accepted, instead of getting ENOTCONN.
                let server_side = UnixSocketState::new();
                server_side.set_path(server.bound_path());
                UnixSocketState::connect_pair(&client, &server_side);
                server.push_accept(server_side);
                return Ok(0);
            }
        }

        file_like.clone().as_socket()?.connect(endpoint).await?;
        Ok(0)
    }

    /// set options for the socket referred to by the file descriptor sockfd.
    pub fn sys_setsockopt(
        &mut self,
        sockfd: usize,
        level: usize,
        optname: usize,
        optval: UserInPtr<u8>,
        optlen: usize,
    ) -> SysResult {
        info!(
            "sys_setsockopt: sockfd:{}, level:{}, optname:{}, optval:{:?} , optlen:{}",
            sockfd, level, optname, optval, optlen
        );
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let socket = file_like.as_socket()?;
        let data = optval.as_slice(optlen)?;
        // `SO_PASSCRED` is answered here rather than inside a socket family,
        // because it is a generic `sock` flag in Linux too (`sock_setsockopt`)
        // and the matching `getsockopt` below has to read back exactly what
        // this stored. See `Socket::set_passcred` for why every family takes
        // it and only AF_UNIX acts on it.
        if level == SOL_SOCKET && optname == SO_PASSCRED {
            // `sock_setsockopt` reads the value as an `int` and refuses a
            // shorter one -- unlike the IP level, which falls back to a byte.
            if data.len() < 4 {
                return Err(LxError::EINVAL);
            }
            let on = u32::from_ne_bytes([data[0], data[1], data[2], data[3]]) != 0;
            return socket.set_passcred(on);
        }
        socket.setsockopt(level, optname, data)
    }

    /// get options for the socket referred to by the file descriptor sockfd.
    pub fn sys_getsockopt(
        &mut self,
        sockfd: usize,
        level: usize,
        optname: usize,
        optval: UserOutPtr<u32>,
        optlen: UserInOutPtr<u32>,
    ) -> SysResult {
        info!(
            "sys_getsockopt: sockfd:{}, level:{}, optname:{}, optval:{:?} , optlen:{:?}",
            sockfd, level, optname, optval, optlen
        );
        let level = match Level::try_from(level) {
            Ok(level) => level,
            Err(_) => {
                // Unknown levels (SOL_PACKET, SOL_IPV6, …) are
                // `ENOPROTOOPT`, same as an unknown optname under a known
                // level. Returning a zeroed int used to tell probes they
                // got a real option value of 0.
                warn!("getsockopt: unsupported level: {}", level);
                return Err(LxError::ENOPROTOOPT);
            }
        };
        if optval.is_null() {
            return Err(LxError::EINVAL);
        }
        match level {
            Level::SOL_SOCKET => {
                // SO_PEERCRED (17): `struct ucred { pid_t pid; uid_t uid;
                // gid_t gid; }` — the credentials of the process on the other end
                // of a connected (unix) socket. seatd reads this to authorize a
                // Wayland client (labwc); without it the call returned ENOPROTOOPT
                // ("invalid optname: 17") and seatd refused the client. The uid
                // and gid are the peer's own, not a constant: dbus-daemon,
                // polkit and logind decide who a client IS from this answer.
                const SO_PEERCRED: usize = 17;
                if optname == SO_PEERCRED {
                    let file_like = self.linux_process().get_file_like(sockfd.into())?;
                    let pid = file_like
                        .as_socket()
                        .ok()
                        .and_then(|s| s.peer_pid())
                        .unwrap_or(1);
                    return write_sockopt_out(optval, optlen, &ucred_of(pid));
                }
                // SO_TYPE (3): SOCK_STREAM / SOCK_DGRAM / ... Python's `ssl`
                // asks it of every socket it wraps (`SSLSocket._create` raises
                // unless the answer is SOCK_STREAM), so without it every HTTPS
                // request from Python -- `requests`, ytmusicapi -- failed with
                // OSError(92, 'Protocol not available').
                // SO_PASSCRED (16): the flag `setsockopt` stored. crashpad
                // READS it before setting it -- the handler may not be allowed
                // to set it and does not need to if the client already did
                // (`InstallClientSocket`) -- and a `getsockopt` that answers
                // ENOPROTOOPT there is a hard `return false`, so every
                // chromium process logged
                //     ERROR:exception_handler_server.cc:361 getsockopt:
                //     Protocol not available (92)
                // and started with no crash handler at all.
                if optname == SO_PASSCRED {
                    let file_like = self.linux_process().get_file_like(sockfd.into())?;
                    let on = u32::from(file_like.as_socket()?.passcred());
                    return write_sockopt_out(optval, optlen, &on.to_ne_bytes());
                }
                const SO_TYPE: usize = 3;
                if optname == SO_TYPE {
                    let file_like = self.linux_process().get_file_like(sockfd.into())?;
                    let ty = file_like
                        .as_socket()?
                        .socket_type()
                        .ok_or(LxError::ENOPROTOOPT)?;
                    return write_sockopt_out(optval, optlen, &(ty as u32).to_ne_bytes());
                }
                let optname = match SolOptname::try_from(optname) {
                    Ok(optname) => optname,
                    Err(_) => {
                        // ENOPROTOOPT is the right answer, and for some of
                        // these it is the answer mainline Linux gives too --
                        // so do not shout about it. dbus, polkit and logind
                        // ask every AF_UNIX peer for its security context
                        // (SO_PEERSEC) and supplementary groups
                        // (SO_PEERGROUPS) on connect; a kernel with no LSM
                        // returns ENOPROTOOPT for the first and one older
                        // than 4.13 for the second, and every caller falls
                        // back cleanly. Logging those at `error!` filled the
                        // console with a repeating "invalid optname: 31 /
                        // invalid optname: 59" that reads like a fault and is
                        // not one.
                        const SO_PEERSEC: usize = 31;
                        const SO_PEERGROUPS: usize = 59;
                        match optname {
                            SO_PEERSEC | SO_PEERGROUPS => debug!(
                                "getsockopt: SOL_SOCKET optname {} (peer identity) not supported",
                                optname
                            ),
                            _ => warn!("getsockopt: unsupported SOL_SOCKET optname: {}", optname),
                        }
                        return Err(LxError::ENOPROTOOPT);
                    }
                };

                let file_like = self.linux_process().get_file_like(sockfd.into())?;
                // Only the buffer-size options need the capacities, and only
                // TCP and UDP report any: the trait default is `None`, so
                // asking unconditionally and unwrapping panicked the KERNEL on
                // a plain `getsockopt(SO_SNDBUF)` against a Unix socket --
                // which is what Firefox does to its own IPC channels.
                // Linux answers with net.core.{w,r}mem_default for a socket
                // with no queue of its own, so answer that.
                const MEM_DEFAULT: usize = 212_992;
                let buffer_capacity = || {
                    file_like
                        .clone()
                        .as_socket()
                        .ok()
                        .and_then(|s| s.get_buffer_capacity())
                        .unwrap_or((MEM_DEFAULT, MEM_DEFAULT))
                };

                match optname {
                    SolOptname::TYPE => {
                        // `SO_TYPE` is mandatory on every socket; without it
                        // glibc and many libraries treat the fd as broken.
                        let ty = file_like
                            .clone()
                            .as_socket()?
                            .socket_type()
                            .ok_or(LxError::ENOPROTOOPT)? as u32;
                        write_sockopt_out(optval, optlen, &ty.to_ne_bytes())
                    }
                    SolOptname::SNDBUF => {
                        let (_, send_buf_ca) = buffer_capacity();
                        write_sockopt_out(optval, optlen, &(send_buf_ca as u32).to_ne_bytes())
                    }
                    SolOptname::RCVBUF => {
                        let (recv_buf_ca, _) = buffer_capacity();
                        write_sockopt_out(optval, optlen, &(recv_buf_ca as u32).to_ne_bytes())
                    }
                    SolOptname::REUSEADDR => {
                        // Report the flag `setsockopt` stored — hardcoding 1
                        // made `getsockopt` lie after `setsockopt(..., 0)` and
                        // disagreed with Linux's default of 0.
                        let on = file_like.clone().as_socket()?.so_reuseaddr();
                        write_sockopt_out(optval, optlen, &(u32::from(on)).to_ne_bytes())
                    }
                    SolOptname::BROADCAST => {
                        // `setsockopt` accepts this (opt 6); without the enum
                        // arm `getsockopt` was `ENOPROTOOPT`. Default is off;
                        // the set path is still a no-op success.
                        write_sockopt_out(optval, optlen, &0u32.to_ne_bytes())
                    }
                    SolOptname::KEEPALIVE => {
                        // Same asymmetry as BROADCAST: set accepts opt 9, get
                        // used to be ENOPROTOOPT. Default off; set is still a
                        // no-op until we plumb smoltcp keepalives.
                        write_sockopt_out(optval, optlen, &0u32.to_ne_bytes())
                    }
                    SolOptname::ERROR => {
                        let err = file_like.clone().as_socket()?.take_so_error();
                        write_sockopt_out(optval, optlen, &(err as u32).to_ne_bytes())
                    }
                    // struct linger { int l_onoff; int l_linger; } — zero-linger.
                    SolOptname::LINGER => write_sockopt_out(optval, optlen, &[0u8; 8]),
                    SolOptname::REUSEPORT => {
                        // `setsockopt` accepts opt 15; without the enum arm
                        // `getsockopt` was ENOPROTOOPT. Default off; set is a
                        // no-op until we plumb reuseport into the bind tables.
                        write_sockopt_out(optval, optlen, &0u32.to_ne_bytes())
                    }
                    // struct timeval { time_t tv_sec; suseconds_t tv_usec; }
                    // — 16 bytes on x86_64; zero means "no timeout" (default).
                    SolOptname::RCVTIMEO | SolOptname::SNDTIMEO => {
                        write_sockopt_out(optval, optlen, &[0u8; 16])
                    }
                    SolOptname::ACCEPTCONN => {
                        // Whether `listen(2)` put this socket in the passive
                        // state. Without the enum arm, `getsockopt` was
                        // ENOPROTOOPT even for a listening TCP/UNIX socket.
                        let on = file_like.clone().as_socket()?.is_listening();
                        write_sockopt_out(optval, optlen, &(u32::from(on)).to_ne_bytes())
                    }
                }
            }
            Level::IPPROTO_TCP => {
                let optname = match TcpOptname::try_from(optname) {
                    Ok(optname) => optname,
                    Err(_) => {
                        warn!("getsockopt: unsupported IPPROTO_TCP optname: {}", optname);
                        return Err(LxError::ENOPROTOOPT);
                    }
                };
                let file_like = self.linux_process().get_file_like(sockfd.into())?;
                let sock = file_like.as_socket()?;
                // `do_tcp_getsockopt` — not STREAM alone (AF_UNIX is STREAM).
                if !sock.is_tcp() {
                    return Err(LxError::ENOPROTOOPT);
                }
                match optname {
                    TcpOptname::NODELAY => {
                        let on = sock.tcp_nodelay();
                        write_sockopt_out(optval, optlen, &(u32::from(on)).to_ne_bytes())
                    }
                    // Linux defaults (`tcp_keepalive_*` sysctls). `setsockopt`
                    // already accepts these; without the enum arms `getsockopt`
                    // was ENOPROTOOPT. Values are stubs until smoltcp keepalives
                    // are plumbed.
                    TcpOptname::KEEPIDLE => {
                        write_sockopt_out(optval, optlen, &7200u32.to_ne_bytes())
                    }
                    TcpOptname::KEEPINTVL => {
                        write_sockopt_out(optval, optlen, &75u32.to_ne_bytes())
                    }
                    TcpOptname::KEEPCNT => write_sockopt_out(optval, optlen, &9u32.to_ne_bytes()),
                    TcpOptname::CONGESTION => {
                        // Linux returns a NUL-terminated CCA name. We have no
                        // pluggable congestion control; answer a fixed "reno"
                        // rather than Ok(0) with an untouched user buffer
                        // (and without requiring a socket → ENOTSOCK).
                        write_sockopt_out(optval, optlen, b"reno\0")
                    }
                }
            }
            Level::IPPROTO_IP => {
                let optname = match IpOptname::try_from(optname) {
                    Ok(optname) => optname,
                    Err(_) => {
                        warn!("getsockopt: unsupported IPPROTO_IP optname: {}", optname);
                        return Err(LxError::ENOPROTOOPT);
                    }
                };
                let file_like = self.linux_process().get_file_like(sockfd.into())?;
                let sock = file_like.as_socket()?;
                // `do_ip_getsockopt` is inet-only (not UNIX/netlink/packet).
                if !sock.is_inet() {
                    return Err(LxError::ENOPROTOOPT);
                }
                match optname {
                    IpOptname::TOS => write_sockopt_out(optval, optlen, &0u32.to_ne_bytes()),
                    IpOptname::TTL => write_sockopt_out(optval, optlen, &64u32.to_ne_bytes()),
                    IpOptname::HDRINCL => {
                        // Only SOCK_RAW IPv4; others get ENOPROTOOPT like Linux.
                        if !sock.is_raw_ipv4() {
                            return Err(LxError::ENOPROTOOPT);
                        }
                        let on = sock.ip_hdrincl();
                        write_sockopt_out(optval, optlen, &(u32::from(on)).to_ne_bytes())
                    }
                    IpOptname::MulticastIf => {
                        write_sockopt_out(optval, optlen, &0u32.to_ne_bytes())
                    }
                    IpOptname::MulticastTtl => {
                        write_sockopt_out(optval, optlen, &1u32.to_ne_bytes())
                    }
                    IpOptname::MulticastLoop => {
                        write_sockopt_out(optval, optlen, &1u32.to_ne_bytes())
                    }
                }
            }
        }
    }

    /// Queue `data` on the socket behind `file_like` the way
    /// `unix_stream_sendmsg` does for a caller that may block: every byte, or
    /// the count that went in before the error, and `SIGPIPE` with a bare
    /// `EPIPE` unless the call opted out.
    async fn send_all(
        &self,
        file_like: &Arc<dyn FileLike>,
        data: &[u8],
        endpoint: Option<Endpoint>,
        mode: SendMode,
    ) -> SysResult {
        use crate::file::{after_write_error, sigpipe_due, AfterWriteError};
        let socket = file_like.as_socket()?;
        let mut sent = 0usize;
        loop {
            match socket.write(&data[sent..], endpoint.clone()) {
                Ok(n) => {
                    sent += n;
                    if !mode.wait || n == 0 || sent >= data.len() {
                        return Ok(sent);
                    }
                }
                // The same three answers `write(2)` gives a pipe.
                Err(e) => match after_write_error(e, mode.wait, sent) {
                    AfterWriteError::Wait => {
                        file_like.async_poll(PollEvents::OUT).await?;
                    }
                    AfterWriteError::Partial => return Ok(sent),
                    AfterWriteError::Fail => {
                        if sigpipe_due(e, mode.sigpipe) {
                            self.thread
                                .lock_linux()
                                .signals
                                .insert(linux_object::signal::Signal::SIGPIPE);
                        }
                        return Err(e);
                    }
                },
            }
        }
    }

    /// The send mode for `file_like` under `flags`.
    fn send_mode_for(&self, file_like: &Arc<dyn FileLike>, flags: usize) -> SendMode {
        send_mode(
            flags,
            file_like.flags().contains(OpenFlags::NON_BLOCK),
            queue_is_bounded(file_like),
        )
    }

    /// transmit a message to another socket
    pub async fn sys_sendto(
        &mut self,
        sockfd: usize,
        buf: UserInPtr<u8>,
        len: usize,
        flags: usize,
        dest_addr: UserInPtr<SockAddr>,
        addrlen: usize,
    ) -> SysResult {
        info!(
            "sys_sendto: sockfd:{:?}, buffer:{:?}, length:{:?}, flags:{:?} , optlen:{:?}, addrlen:{:?}",
            sockfd, buf, len, flags, dest_addr, addrlen
        );
        let endpoint = if dest_addr.is_null() {
            None
        } else {
            let endpoint =
                sockaddr_to_endpoint(read_sockaddr(dest_addr.as_addr(), addrlen)?, addrlen)?;
            info!("sys_sendto: sockfd:{:?}, endpoint:{:?}", sockfd, endpoint);
            Some(endpoint)
        };
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        // Return the socket's ACTUAL queued byte count, not the full requested
        // `len`. TCP `write()` can perform a short write (queues min(len, free TX
        // space)); reporting `len` regardless makes the caller believe bytes it
        // never sent were delivered, silently truncating the stream.
        let mode = self.send_mode_for(&file_like, flags);
        let written = self
            .send_all(&file_like, buf.as_slice(len)?, endpoint, mode)
            .await?;
        // Do not drain_net_poll here — busybox ping uses sendto; 32× poll_ifaces
        // blocks for a long time (smoltcp + SOCKETS lock). Sockets drive RX in read/poll.
        Ok(written)
    }

    /// receive messages from a socket
    pub async fn sys_recvfrom(
        &mut self,
        sockfd: usize,
        mut buf: UserOutPtr<u8>,
        len: usize,
        flags: usize,
        src_addr: UserOutPtr<SockAddr>,
        addrlen: UserInOutPtr<u32>,
    ) -> SysResult {
        let _ = self.maybe_handle_tty_intr()?;
        linux_object::process::check_signals()?;
        info!(
            "sys_recvfrom: sockfd:{}, buffer:{:?}, length:{}, flags:{} , src_addr:{:?}, addrlen:{:?}",
            sockfd, buf, len, flags, src_addr, addrlen
        );
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let old_flags = file_like.flags();
        let force_nonblock =
            (flags & MSG_DONTWAIT) != 0 && !old_flags.contains(OpenFlags::NON_BLOCK);
        if force_nonblock {
            file_like.set_flags(old_flags | OpenFlags::NON_BLOCK)?;
        }
        debug!("FileLike {} flags: {:?}", sockfd, file_like.flags());
        let cap_len = len.min(super::SYSCALL_IO_MAX);
        let mut data = vec![0u8; cap_len];
        let socket = file_like.as_socket()?;
        let (result, endpoint) = if (flags & MSG_PEEK) != 0 {
            socket.peek(&mut data).await
        } else {
            socket.read(&mut data).await
        };
        if force_nonblock {
            let _ = file_like.set_flags(old_flags);
        }
        if let Ok(received) = result {
            if !src_addr.is_null() {
                let sockaddr_in = SockAddr::from(endpoint);
                sockaddr_in.write_to(src_addr, addrlen)?;
            }
            buf.write_array(&data[..received])?;
            Ok(received)
        } else {
            result
        }
    }

    /// Parse `SCM_RIGHTS` ancillary data and resolve the carried fd numbers to
    /// the sender's open files, ready to be queued on the peer.
    ///
    /// The byte walk is [`parse_scm_rights_fds`]; this half only turns numbers
    /// into files. An fd the sender does not have is `EBADF` for the **whole**
    /// `sendmsg`, as `scm_fp_copy()` does. Skipping it silently, which is what
    /// this used to do, is the worse answer by far: the receiver gets a short
    /// fd array with no indication, so a protocol that pairs the Nth fd with
    /// the Nth request — every one of them: DRI3, `wl_shm`, `wl_buffer` —
    /// silently pairs each fd with somebody else's request from there on.
    fn collect_scm_rights_fds(&self, ctrl: &[u8]) -> Result<Vec<Arc<dyn FileLike>>, LxError> {
        let raw_fds = parse_scm_rights_fds(ctrl)?;
        let proc = self.linux_process();
        let mut fds = Vec::with_capacity(raw_fds.len());
        for raw in raw_fds {
            if raw < 0 {
                return Err(LxError::EBADF);
            }
            fds.push(proc.get_file_like(FileDesc::from(raw as usize))?);
        }
        Ok(fds)
    }

    /// transmit a message to another socket
    #[allow(unsafe_code)]
    pub async fn sys_sendmsg(
        &mut self,
        sockfd: usize,
        msg: UserInPtr<MsgHdr>,
        flags: usize,
    ) -> SysResult {
        info!(
            "sys_sendmsg: sockfd:{:?}, msg:{:?}, flags:{}",
            sockfd, msg, flags
        );
        let hdr = msg.read()?;
        self.sendmsg_hdr(sockfd, &hdr, flags).await
    }

    /// Core of `sendmsg` for an already-read header (shared with `sendmmsg`).
    async fn sendmsg_hdr(&mut self, sockfd: usize, hdr: &MsgHdr, flags: usize) -> SysResult {
        let iov_ptr: UserInPtr<IoVecIn> = hdr.msg_iov.as_addr().into();
        let iovlen = hdr.msg_iovlen;
        let iovs = iov_ptr.read_iovecs(iovlen)?;
        let total = iovs.total_len();

        // Resolve the fd before gathering, so the socket's type can decide what
        // an oversized message means (and so EBADF/ENOTSOCK wins over a size
        // complaint, as it does on Linux).
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let sock_type = file_like.as_socket()?.socket_type();

        // A message longer than the bounded kernel buffer is NOT an error on a
        // stream socket. `sendmsg` may return a short count exactly like
        // `write`, and rejecting the whole call with `EINVAL` is what the
        // `sys_writev` doc block above describes killing every GLX client: xcb
        // saw errno 22, marked the connection dead and exited with "XIO: fatal
        // IO error 22 (Invalid argument)" right after its window appeared.
        // Firefox dies the same way, one layer up — its IPC channel treats any
        // `sendmsg` error as fatal and tears the channel down, so the browser
        // window opens and then immediately goes away.
        //
        // So gather only the first `SYSCALL_IO_MAX` bytes and report that count;
        // the caller resumes from it (Mozilla IPC, libwayland and stdio all
        // track a partial-write offset).
        //
        // Only for a socket we KNOW is a stream, though. Anything
        // message-oriented gets `EMSGSIZE`: a message boundary means a short
        // write would corrupt the message rather than short-change the writer,
        // and `EMSGSIZE` is what Linux returns for a message too large to send
        // atomically. `None` counts as message-oriented on purpose — it means
        // the socket does not report a type (netlink is one), and guessing
        // "stream" there would silently split, say, a netlink dump. Erring
        // toward a clean error beats erring toward silent corruption.
        let data = if total > super::SYSCALL_IO_MAX {
            if !matches!(sock_type, Some(SocketType::SOCK_STREAM)) {
                return Err(LxError::EMSGSIZE);
            }
            let mut buf = alloc::vec![0u8; super::SYSCALL_IO_MAX];
            let n = iovs.read_bytes_at(0, &mut buf)?;
            buf.truncate(n);
            buf
        } else {
            iovs.read_to_vec()?
        };

        // SCM_RIGHTS: resolve any attached fds before queueing the bytes.
        // `msg_controllen` is fully user-controlled; bound it before `read_array`
        // so a huge value cannot request a multi-GiB allocation (alloc abort) or
        // walk off the mapped control buffer. Linux bounds this by optmem_max.
        const CONTROL_MAX: usize = 64 * 1024;
        let passed_fds = if !hdr.msg_control.is_null() && hdr.msg_controllen >= 16 {
            if hdr.msg_controllen > CONTROL_MAX {
                return Err(LxError::EINVAL);
            }
            let ctrl = hdr.msg_control.read_array(hdr.msg_controllen)?;
            self.collect_scm_rights_fds(&ctrl)?
        } else {
            Vec::new()
        };

        let endpoint = if !hdr.msg_name.is_null() {
            let namelen = hdr.msg_namelen as usize;
            let endpoint =
                sockaddr_to_endpoint(read_sockaddr(hdr.msg_name.as_addr(), namelen)?, namelen)?;
            Some(endpoint)
        } else {
            None
        };

        let socket_fl = file_like.clone();
        let socket = socket_fl.as_socket()?;
        // Return the actual queued byte count (a TCP short write can queue less
        // than `data.len()`); reporting the full length silently drops the tail.
        // Queue the SCM_RIGHTS fds BEFORE the bytes so they are tagged with the
        // byte offset of the FIRST byte of this message. Linux delivers a passed
        // fd together with the recvmsg that returns the first accompanying data
        // byte; queueing after the write tagged the fds at the message's END, so
        // a peer that reads a header first and the body second (seatd/libseat)
        // got "Bad file descriptor" — the fd was withheld until the whole
        // message had been read.
        let fds_queued = !passed_fds.is_empty();
        if fds_queued {
            // Do not swallow the error: on a non-unix socket SCM_RIGHTS must
            // fail the whole `sendmsg` (`EOPNOTSUPP`), not queue nothing and
            // still send the bytes.
            socket.send_fds(passed_fds)?;
        }
        let mode = self.send_mode_for(&file_like, flags);
        let sent = self.send_all(&file_like, &data, endpoint, mode).await;
        // A message that did not go in takes its descriptors with it, as the
        // skb they rode on is freed on Linux. Leaving them queued meant the
        // caller's retry of the same `sendmsg` (the normal answer to EAGAIN
        // on a non-blocking socket) delivered every fd twice, and the peer
        // paired the duplicates with the next messages' requests.
        if fds_queued && sent.is_err() {
            if let Some(unix) = file_like.downcast_ref::<UnixSocketState>() {
                unix.retract_fds();
            }
        }
        sent
    }

    /// receive messages from a socket
    pub async fn sys_recvmsg(
        &mut self,
        sockfd: usize,
        msg: UserInOutPtr<MsgHdr>,
        flags: usize,
    ) -> SysResult {
        info!(
            "sys_recvmsg: sockfd:{}, msg:{:?}, flags:{}",
            sockfd, msg, flags
        );
        let mut hdr = msg.read()?;
        // Capture the user address before `msg` may be moved by `write_to_msg`.
        let msg_addr = msg.as_addr();

        let iov_ptr = hdr.msg_iov;
        let iovlen = hdr.msg_iovlen;
        let mut iovs = iov_ptr.read_iovecs(iovlen)?;
        let total_len = iovs.total_len().min(super::SYSCALL_IO_MAX);
        let mut data = vec![0u8; total_len];

        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let old_flags = file_like.flags();
        let force_nonblock =
            (flags & MSG_DONTWAIT) != 0 && !old_flags.contains(OpenFlags::NON_BLOCK);
        if force_nonblock {
            file_like.set_flags(old_flags | OpenFlags::NON_BLOCK)?;
        }
        let socket = file_like.as_socket()?;
        let (result, endpoint) = if (flags & MSG_PEEK) != 0 {
            socket.peek(&mut data).await
        } else {
            socket.read(&mut data).await
        };
        if force_nonblock {
            let _ = file_like.set_flags(old_flags);
        }

        let addr = hdr.msg_name;
        if let Ok(len) = result {
            iovs.write_from_buf(&data[..len])?;
            if !addr.is_null() {
                let sockaddr_in = SockAddr::from(endpoint);
                sockaddr_in.write_to_msg(msg)?;
            }
            // SCM_RIGHTS: install any fds the peer attached, and SCM_CREDENTIALS
            // when this end asked for credentials, and emit the cmsgs.
            let mut ctrl_written = 0usize;
            let mut ctrunc = false;
            if !hdr.msg_control.is_null() && hdr.msg_controllen >= CMSG_HDR_LEN {
                // Credentials, when wanted, are reserved out of the control
                // buffer BEFORE the descriptors are counted: they are the
                // kernel's own word about who sent this, so dropping them to
                // fit one more fd would quietly answer the receiver's question
                // wrong.
                let creds = if socket.passcred() {
                    // The record stamped when the message was WRITTEN, which
                    // is the only one that answers "who sent this": the writer
                    // need not be whoever created the socket. chromium's
                    // zygote is that case -- it writes through a `socketpair`
                    // end inherited across `fork`, and a `socketpair` records
                    // no creator, so the endpoint's owner is pid 0.
                    //
                    // Falling back to the peer's owner covers a read of bytes
                    // that predate this option being turned on.
                    socket
                        .recv_creds()
                        .or_else(|| socket.peer_pid().map(ucred_of))
                } else {
                    None
                };
                let creds_room = creds.map_or(0, |_| cmsg_align(CMSG_HDR_LEN + 12));
                let fd_room = hdr.msg_controllen.saturating_sub(creds_room);
                let max_fds = fd_room.saturating_sub(CMSG_HDR_LEN) / 4;
                let fds = socket.recv_fds(max_fds);
                let mut installed: Vec<i32> = Vec::with_capacity(fds.len());
                if !fds.is_empty() {
                    let proc = self.linux_process();
                    // Same case as `dup`/`pidfd_getfd`: a new descriptor onto
                    // an already-open description. `add_file` would copy
                    // `O_CLOEXEC` from the FileLike (the sender's bit);
                    // Linux keys it off `MSG_CMSG_CLOEXEC` alone.
                    let cloexec = scm_rights_cloexec(flags);
                    for fl in fds {
                        installed.push(proc.add_file_cloexec(fl, cloexec)?.into());
                    }
                }
                let cbuf = build_recv_cmsgs(&installed, creds.as_ref());
                if cbuf.len() <= hdr.msg_controllen {
                    ctrl_written = cbuf.len();
                    if ctrl_written > 0 {
                        hdr.msg_control.write_array(&cbuf[..ctrl_written])?;
                    }
                } else {
                    // The credentials are what did not fit -- the descriptors
                    // were counted against the room left over for them, and
                    // `recv_fds` honoured that budget. Writing the cmsg
                    // half-way would hand the reader a header whose
                    // `cmsg_len` runs off the end of its own buffer, which is
                    // how a strict parser walks into garbage. So drop the
                    // credentials whole and say so with `MSG_CTRUNC`, as
                    // Linux does.
                    ctrunc = true;
                    let fitting = build_recv_cmsgs(&installed, None);
                    ctrl_written = fitting.len();
                    if ctrl_written > 0 {
                        hdr.msg_control.write_array(&fitting[..ctrl_written])?;
                    }
                }
            }
            // Linux ALWAYS reports how much ancillary data it wrote through
            // msg_controllen — 0 when there is none. Leaving the caller's IN
            // value (the buffer CAPACITY) there handed strict cmsg parsers a
            // buffer's worth of stale user memory to walk as if it were
            // kernel-written control headers: rustix (wayland-rs / lunarbg)
            // read a garbage cmsg_len there and panicked with
            // "range start index 18446744069414584321 out of range for slice
            // of length 136" — 136 being exactly the untouched capacity.
            // libwayland's C macros merely skidded over the same garbage.
            if !hdr.msg_control.is_null() {
                let controllen_addr = msg_addr + core::mem::offset_of!(MsgHdr, msg_controllen);
                let mut p = UserOutPtr::<usize>::from(controllen_addr);
                p.write(ctrl_written)?;
            }
            // Report truncation (and any other recv flags) via msg_flags.
            let msg_flags = socket.take_msg_flags() | if ctrunc { MSG_CTRUNC } else { 0 };
            {
                let flags_addr = msg_addr + core::mem::offset_of!(MsgHdr, msg_flags);
                let mut p = UserOutPtr::<i32>::from(flags_addr);
                p.write(msg_flags)?;
            }
        }

        result
    }

    /// assigns the address specified by addr to the socket referred to by the file descriptor sockfd
    pub fn sys_bind(
        &mut self,
        sockfd: usize,
        addr: UserInPtr<SockAddr>,
        addrlen: usize,
    ) -> SysResult {
        info!(
            "sys_bind: sockfd:{:?}, addr:{:?}, addrlen:{}",
            sockfd, addr, addrlen
        );
        let endpoint = sockaddr_to_endpoint(read_sockaddr(addr.as_addr(), addrlen)?, addrlen)?;
        debug!("sys_bind: fd:{} bind to {:?}", sockfd, endpoint);

        let proc = self.linux_process();
        if let Endpoint::Unix(path) = &endpoint {
            if !path.is_empty() {
                // Abstract-namespace sockets (leading NUL, e.g. X11's
                // `\0/tmp/.X11-unix/X0`) live only in the in-kernel registry and
                // have no filesystem node; only pathname sockets get a node.
                let is_abstract = path.starts_with('\0');
                if !is_abstract {
                    let (dir_path, file_name) = split_path(path);
                    match proc.lookup_inode_at(FileDesc::CWD, dir_path, true) {
                        Ok(dir_inode) => {
                            if dir_inode.find(file_name).is_err() {
                                if let Err(err) = dir_inode.create(
                                    file_name,
                                    linux_object::fs::vfs::FileType::Socket,
                                    0o666,
                                ) {
                                    warn!(
                                        "sys_bind: unable to create unix socket node {:?}: {:?}; continuing with in-kernel registration only",
                                        file_name, err
                                    );
                                } else {
                                    linux_object::fs::dcache_invalidate();
                                }
                            }
                        }
                        Err(err) => {
                            warn!(
                                "sys_bind: unable to lookup unix socket directory {:?}: {:?}; continuing with in-kernel registration only",
                                dir_path, err
                            );
                        }
                    }
                }

                let file_like = proc.get_file_like(sockfd.into())?;
                if let Ok(unix) = file_like.clone().downcast_arc::<UnixSocketState>() {
                    // Refuse before `register`: otherwise a second bind to a
                    // different path would claim two registry slots.
                    if !unix.bound_path().is_empty() {
                        return Err(LxError::EINVAL);
                    }
                    UnixSocketState::register(path.clone(), unix)?;
                }
            }
        }

        let file_like = proc.get_file_like(sockfd.into())?;
        file_like.clone().as_socket()?.bind(endpoint)
    }

    /// marks the socket referred to by sockfd as a passive socket,
    /// that is, as a socket that will be used to accept incoming connection
    pub fn sys_listen(&mut self, sockfd: usize, backlog: usize) -> SysResult {
        info!("sys_listen: fd:{}, backlog:{}", sockfd, backlog);
        // smoltcp tcp sockets do not support backlog
        // open multiple sockets for each connection
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        file_like.clone().as_socket()?.listen()
    }

    /// shutdown a socket
    pub fn sys_shutdown(&mut self, sockfd: usize, howto: usize) -> SysResult {
        info!("sys_shutdown: sockfd:{}, howto:{}", sockfd, howto);
        // `__sys_shutdown_sock`: `how > SHUT_RDWR` is `EINVAL` before the
        // protocol handler runs. Netlink (and any other socket that used to
        // ignore `howto`) must not turn `shutdown(fd, 99)` into success.
        if howto > 2 {
            return Err(LxError::EINVAL);
        }
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        file_like.clone().as_socket()?.shutdown(howto)
    }

    /// accept() is used with connection-based socket types (SOCK_STREAM, SOCK_SEQPACKET).
    /// It extracts the first connection request on the queue of pending connections
    /// for the listening socket, sockfd, creates a new connected socket, and returns
    /// a new file descriptor referring to that socket.
    /// The newly created socket is not in the listening state.
    /// The original socket sockfd is unaffected by this call.
    pub async fn sys_accept(
        &mut self,
        sockfd: usize,
        addr: UserOutPtr<SockAddr>,
        addrlen: UserInOutPtr<u32>,
    ) -> SysResult {
        self.sys_accept4(sockfd, addr, addrlen, 0).await
    }

    /// Like [`Self::sys_accept`], but takes an extra `flags` argument that may
    /// carry `SOCK_NONBLOCK` (`0x800`) and/or `SOCK_CLOEXEC` (`0x80000`),
    /// applied to the newly accepted socket. These share the bit values of
    /// `O_NONBLOCK`/`O_CLOEXEC`, so they map directly onto [`OpenFlags`].
    pub async fn sys_accept4(
        &mut self,
        sockfd: usize,
        addr: UserOutPtr<SockAddr>,
        addrlen: UserInOutPtr<u32>,
        flags: usize,
    ) -> SysResult {
        info!(
            "sys_accept4: sockfd:{}, addr:{:?}, addrlen={:?}, flags={:#x}",
            sockfd, addr, addrlen, flags
        );
        // Validate flags BEFORE accept() consumes a connection from the queue.
        // SOCK_NONBLOCK / SOCK_CLOEXEC requested for the accepted socket; any
        // other bit is invalid (GLib's GDBus path only ever passes these two).
        // Previously this ran after accept(), so a bad flag accepted then
        // dropped an established client connection.
        let new_flags = anon_fd_flags(flags, ANON_CLOEXEC | ANON_NONBLOCK)?;

        // smoltcp tcp sockets do not support backlog
        // open multiple sockets for each connection
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let (new_socket, remote_endpoint) = file_like.clone().as_socket()?.accept().await?;
        debug!(
            "FileLike{} flags: {:?}, New flags: {:?}",
            sockfd,
            file_like.flags(),
            new_socket.flags()
        );

        if new_flags.bits() != 0 {
            new_socket.set_flags(new_flags)?;
        }

        // Copy the peer address out BEFORE installing the fd, so a bad addr/
        // addrlen pointer fails the syscall (EFAULT) without leaking the
        // accepted fd into the process table (Linux copies the address out
        // before fd_install).
        if !addr.is_null() {
            let sockaddr_in = SockAddr::from(remote_endpoint);
            sockaddr_in.write_to(addr, addrlen)?;
        }
        let new_fd = self.linux_process().add_socket(new_socket)?;
        Ok(new_fd.into())
    }

    /// returns the current address to which the socket sockfd is bound,
    /// in the buffer pointed to by addr.
    pub fn sys_getsockname(
        &mut self,
        sockfd: usize,
        addr: UserOutPtr<SockAddr>,
        addrlen: UserInOutPtr<u32>,
    ) -> SysResult {
        info!(
            "sys_getsockname: sockfd:{}, addr:{:?}, addrlen:{:?}",
            sockfd, addr, addrlen
        );
        if addr.is_null() {
            return Err(LxError::EINVAL);
        }
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let endpoint = file_like
            .clone()
            .as_socket()?
            .endpoint()
            .ok_or(LxError::EINVAL)?;
        SockAddr::from(endpoint).write_to(addr, addrlen)?;
        Ok(0)
    }

    /// returns the address of the peer connected to the socket sockfd,
    /// in the buffer pointed to by addr.
    pub fn sys_getpeername(
        &mut self,
        sockfd: usize,
        addr: UserOutPtr<SockAddr>,
        addrlen: UserInOutPtr<u32>,
    ) -> SysResult {
        info!(
            "sys_getpeername: sockfd:{}, addr:{:?}, addrlen:{:?}",
            sockfd, addr, addrlen
        );
        // smoltcp tcp sockets do not support backlog
        // open multiple sockets for each connection
        if addr.is_null() {
            return Err(LxError::EINVAL);
        }
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let remote_endpoint = peer_endpoint(file_like.clone().as_socket()?.remote_endpoint())?;
        SockAddr::from(remote_endpoint).write_to(addr, addrlen)?;
        Ok(0)
    }

    /// creates a pair of connected sockets in the specified domain, of the specified type,
    /// and using the optionally specified protocol.
    pub fn sys_socketpair(
        &mut self,
        domain: usize,
        _type: usize,
        protocol: usize,
        mut sv: UserOutPtr<i32>,
    ) -> SysResult {
        info!(
            "sys_socketpair: domain:{}, type:{}, protocol:{}",
            domain, _type, protocol
        );
        if domain != Domain::AF_UNIX as usize {
            return Err(LxError::EAFNOSUPPORT);
        }
        // Same gate as `sys_socket`: an unrecognized type is `EINVAL`, not a
        // silent SOCK_STREAM pair. AF_UNIX only knows STREAM/DGRAM/SEQPACKET
        // (`unix_create`); anything else named by `SocketType` is
        // `EOPNOTSUPP` (Linux's `ESOCKTNOSUPPORT`).
        let socket_type = unix_socketpair_type(_type & SOCKET_TYPE_MASK)?;
        // `unix_create`: non-zero protocol other than PF_UNIX is
        // `EPROTONOSUPPORT`. The argument used to be logged and ignored.
        unix_protocol(protocol)?;
        let proc = self.linux_process();
        let socket1 = Arc::new(UnixSocketState::default());
        let socket2 = Arc::new(UnixSocketState::default());
        UnixSocketState::connect_pair(&socket1, &socket2);
        socket1.set_socket_type(socket_type);
        socket2.set_socket_type(socket_type);
        // The type argument packs SOCK_NONBLOCK / SOCK_CLOEXEC alongside the
        // socket type (same bit values as O_NONBLOCK / O_CLOEXEC, like
        // accept4). These used to be silently dropped, handing out BLOCKING
        // sockets to callers whose event loops assume nonblocking semantics —
        // Firefox's WaylandProxy (socketpair(AF_UNIX, SOCK_STREAM |
        // SOCK_NONBLOCK | SOCK_CLOEXEC)) drains with read-until-EAGAIN, so a
        // blocking pair wedged its forwarding thread and Wayland startup died
        // with "ProxiedConnection: broken source socket". Masking only the
        // two known bits also hid any other flag as success; validate like
        // `sys_socket` / `accept4`.
        let new_flags = anon_fd_flags(_type & !SOCKET_TYPE_MASK, ANON_CLOEXEC | ANON_NONBLOCK)?;
        if new_flags.bits() != 0 {
            socket1.set_flags(new_flags)?;
            socket2.set_flags(new_flags)?;
        }
        let fd1 = proc.add_socket(socket1)?;
        // Taken back if the caller never gets the numbers, like `pipe2`.
        hand_out_pair(
            fd1,
            || proc.add_socket(socket2),
            |fd1, fd2| {
                sv.write_array(&[fd1.into(), fd2.into()])?;
                Ok(())
            },
            |fd| {
                let _ = proc.close_file(fd);
            },
        )?;
        Ok(0)
    }

    /// `sendmmsg`: send an array of messages in one call. glibc's resolver
    /// fires its parallel A+AAAA DNS queries through this; without it every
    /// Firefox name lookup spammed `unknown syscall: SENDMMSG` and fell back.
    /// Sequential delegation to the sendmsg core; each entry's `msg_len` is
    /// written back. On error: fail if nothing was sent, else report the count.
    pub async fn sys_sendmmsg(
        &mut self,
        sockfd: usize,
        msgvec: UserInOutPtr<u8>,
        vlen: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "sys_sendmmsg: sockfd:{}, msgvec:{:?}, vlen:{}",
            sockfd, msgvec, vlen
        );
        const MMSG_MAX: usize = 64;
        let base = msgvec.as_addr();
        let stride = core::mem::size_of::<MsgHdr>() + 8; // + u32 msg_len + pad
        let mut sent = 0usize;
        for i in 0..vlen.min(MMSG_MAX) {
            let hdr_ptr: UserInPtr<MsgHdr> = (base + i * stride).into();
            let hdr = hdr_ptr.read()?;
            match self.sendmsg_hdr(sockfd, &hdr, flags).await {
                Ok(n) => {
                    let mut len_ptr: UserOutPtr<u32> =
                        (base + i * stride + core::mem::size_of::<MsgHdr>()).into();
                    len_ptr.write(n as u32)?;
                    sent += 1;
                }
                Err(e) => {
                    if sent == 0 {
                        return Err(e);
                    }
                    break;
                }
            }
        }
        Ok(sent)
    }

    /// `recvmmsg`: receive up to `vlen` messages. The first receive honors the
    /// caller's blocking mode; subsequent ones are forced non-blocking so the
    /// call returns as soon as the queue drains (Linux semantics without the
    /// timeout extra, which the resolver does not rely on).
    pub async fn sys_recvmmsg(
        &mut self,
        sockfd: usize,
        msgvec: UserInOutPtr<u8>,
        vlen: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "sys_recvmmsg: sockfd:{}, msgvec:{:?}, vlen:{}",
            sockfd, msgvec, vlen
        );
        const MMSG_MAX: usize = 64;
        let base = msgvec.as_addr();
        let stride = core::mem::size_of::<MsgHdr>() + 8;
        let mut received = 0usize;
        for i in 0..vlen.min(MMSG_MAX) {
            let hdr_ptr: UserInOutPtr<MsgHdr> = (base + i * stride).into();
            let per_flags = if i == 0 { flags } else { flags | MSG_DONTWAIT };
            match self.sys_recvmsg(sockfd, hdr_ptr, per_flags).await {
                Ok(n) => {
                    let mut len_ptr: UserOutPtr<u32> =
                        (base + i * stride + core::mem::size_of::<MsgHdr>()).into();
                    len_ptr.write(n as u32)?;
                    received += 1;
                }
                Err(LxError::EAGAIN) if received > 0 => break,
                Err(e) => {
                    if received == 0 {
                        return Err(e);
                    }
                    break;
                }
            }
        }
        Ok(received)
    }

    /// Eclipse-specific DNS/hosts lookup for userland shims (`libeclipse_dns.so`).
    pub fn sys_eclipse_dns_query(
        &self,
        name: UserInPtr<u8>,
        name_len: usize,
        family: usize,
        out: UserOutPtr<linux_object::net::dns::DnsResultEntry>,
        out_max: usize,
    ) -> SysResult {
        use linux_object::fs::dns_vfs_root;
        use linux_object::net::dns::{self, DnsFamily};

        if out_max == 0 {
            return Ok(0);
        }
        if name_len == 0 || name_len > 253 {
            return Err(LxError::EINVAL);
        }
        let hostname = name.as_str(name_len).map_err(|_| LxError::EINVAL)?;
        let root_inode = dns_vfs_root().ok_or(LxError::ENOENT)?;
        let addrs = dns::resolve(&root_inode, hostname, DnsFamily::from_usize(family))?;
        let n = addrs.len().min(out_max);
        for (i, ip) in addrs.iter().take(n).enumerate() {
            out.add(i)
                .write(linux_object::net::dns::DnsResultEntry::from_ip(*ip))?;
        }
        Ok(n as _)
    }
}

/// AF_UNIX socket types Linux's `unix_create` accepts. Anything else named
/// by `SocketType` is `EOPNOTSUPP` (`ESOCKTNOSUPPORT`).
fn unix_socket_type(t: SocketType) -> Result<SocketType, LxError> {
    match t {
        SocketType::SOCK_STREAM | SocketType::SOCK_DGRAM | SocketType::SOCK_SEQPACKET => Ok(t),
        _ => Err(LxError::EOPNOTSUPP),
    }
}

/// AF_UNIX `socketpair`'s type word (already stripped of SOCK_* flags): the
/// three Linux knows, or the errno it answers for anything else.
///
/// An unrecognized value used to be ignored and the pair born as
/// `SOCK_STREAM`, so `socketpair(AF_UNIX, 99, 0, sv)` succeeded where
/// `socket` and Linux say `EINVAL`. `socket(AF_UNIX, …)` used the same
/// silent fallback until it shared [`unix_socket_type`].
fn unix_socketpair_type(type_bits: usize) -> Result<SocketType, LxError> {
    let t = SocketType::try_from(type_bits).map_err(|_| LxError::EINVAL)?;
    unix_socket_type(t)
}

/// AF_UNIX protocol word (`unix_create`: `if (protocol && protocol !=
/// PF_UNIX) return -EPROTONOSUPPORT`). Zero and `PF_UNIX`/`AF_UNIX` are
/// the only values that mean anything; anything else used to succeed.
fn unix_protocol(protocol: usize) -> Result<(), LxError> {
    if protocol != 0 && protocol != Domain::AF_UNIX as usize {
        return Err(LxError::EPROTONOSUPPORT);
    }
    Ok(())
}

/// `getpeername(2)`: no peer is `ENOTCONN`, not `EINVAL` (that one is for a
/// bad `addr`/`addrlen`). Used to answer `EINVAL`, so callers that probe
/// before `connect` returns branched on the wrong errno.
fn peer_endpoint(ep: Option<Endpoint>) -> Result<Endpoint, LxError> {
    ep.ok_or(LxError::ENOTCONN)
}

/// The unix-socket control path: passing a file descriptor from one process to
/// another, which had no tests at all.
///
/// This is how a graphical desktop hands buffers around — DRI3 passes a GPU
/// buffer's fd over the X11 socket, `wl_shm` passes a memfd over the Wayland
/// one — so everything here runs thousands of times a second in a session and
/// never once in CI, which has no display.
///
/// Every field of a control buffer comes from userspace, `cmsg_len` included,
/// so the walk is written to survive whatever is in it. The round-trip test at
/// the end is the one that matters most: the 16-byte header is written in two
/// places, once by the parser and once by the builder, and nothing but that
/// test keeps them agreeing.
#[cfg(test)]
mod scm_rights_tests {
    use super::*;

    /// A control buffer holding one cmsg with `level`, `typ` and `payload`,
    /// padded to `CMSG_ALIGN` so another can follow.
    fn cmsg(level: i32, typ: i32, payload: &[u8]) -> Vec<u8> {
        let cmsg_len = CMSG_HDR_LEN + payload.len();
        let mut b = Vec::new();
        b.extend_from_slice(&(cmsg_len as u64).to_ne_bytes());
        b.extend_from_slice(&level.to_ne_bytes());
        b.extend_from_slice(&typ.to_ne_bytes());
        b.extend_from_slice(payload);
        while b.len() % 8 != 0 {
            b.push(0);
        }
        b
    }

    /// The bytes of `fds` as a C `int` array.
    fn fd_bytes(fds: &[i32]) -> Vec<u8> {
        fds.iter().flat_map(|f| f.to_ne_bytes()).collect()
    }

    /// A cmsg with a hand-chosen `cmsg_len`, for the malformed cases.
    fn cmsg_raw(cmsg_len: u64, level: i32, typ: i32, payload: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&cmsg_len.to_ne_bytes());
        b.extend_from_slice(&level.to_ne_bytes());
        b.extend_from_slice(&typ.to_ne_bytes());
        b.extend_from_slice(payload);
        b
    }

    #[test]
    fn one_message_yields_the_descriptors_it_carries() {
        let ctrl = cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[7, 9, 11]));
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![7, 9, 11]));
    }

    /// `MSG_CMSG_CLOEXEC` is the only bit that decides FD_CLOEXEC on the
    /// installed descriptors — not the sender's `O_CLOEXEC` on the FileLike.
    #[test]
    fn msg_cmsg_cloexec_is_the_install_bit() {
        assert!(!scm_rights_cloexec(0));
        assert!(!scm_rights_cloexec(MSG_DONTWAIT | MSG_PEEK));
        assert!(scm_rights_cloexec(MSG_CMSG_CLOEXEC));
        assert!(scm_rights_cloexec(MSG_CMSG_CLOEXEC | MSG_DONTWAIT));
        assert_eq!(MSG_CMSG_CLOEXEC, 0x4000_0000);
    }

    #[test]
    fn a_message_of_another_level_or_type_carries_no_descriptors() {
        // SCM_CREDENTIALS (type 2) and IPPROTO_IP (level 0) both hold plain
        // data; reading it as descriptor numbers would install whatever the
        // numbers happened to be.
        let creds = cmsg(SOL_SOCKET_LEVEL, 2, &fd_bytes(&[3, 4, 5]));
        assert_eq!(parse_scm_rights_fds(&creds), Ok(vec![]));
        let ip = cmsg(0, SCM_RIGHTS, &fd_bytes(&[3, 4, 5]));
        assert_eq!(parse_scm_rights_fds(&ip), Ok(vec![]));
    }

    #[test]
    fn several_messages_are_walked_in_order_across_the_padding() {
        // The first payload is 4 bytes, so its cmsg is 20 and the next one
        // starts at 24. Getting the alignment wrong reads the second header
        // out of the middle of the first message.
        let mut ctrl = cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3]));
        assert_eq!(ctrl.len(), 24);
        ctrl.extend(cmsg(SOL_SOCKET_LEVEL, 2, &fd_bytes(&[99])));
        ctrl.extend(cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[5, 6])));
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![3, 5, 6]));
    }

    /// `SO_PASSCRED` is why chromium's zygote works at all: the browser hands
    /// the zygote a `SOCK_SEQPACKET` pair, the forked child pings it, and the
    /// browser reads the child's REAL pid out of `SCM_CREDENTIALS`
    /// (`RecvMsgWithPid`). With no credentials message the pid stayed -1, the
    /// browser sent that -1 back, and the zygote answered
    ///     Zygote could not fork: process_type gpu-process numfds 6 child_pid -1
    /// after killing a child that had forked perfectly well.
    #[test]
    fn the_credentials_message_is_the_one_scm_rights_is_not() {
        let creds = ucred_of(1282);
        let buf = build_recv_cmsgs(&[], Some(&creds));
        assert_eq!(buf.len(), CMSG_HDR_LEN + 12);
        assert_eq!(
            i32::from_ne_bytes(buf[12..16].try_into().unwrap()),
            SCM_CREDENTIALS
        );
        assert_eq!(
            u64::from_ne_bytes(buf[0..8].try_into().unwrap()) as usize,
            CMSG_HDR_LEN + 12,
            "cmsg_len counts the header and the ucred, not the padding"
        );
        assert_eq!(buf[16..28], creds[..], "the ucred rides in the payload");
        // It is NOT a descriptor message, so the fd walk must not take it for
        // one -- a `ucred` read as fd numbers would install whatever pid, uid
        // and gid happened to be.
        assert_eq!(parse_scm_rights_fds(&buf), Ok(vec![]));
        // And with the option off there is no message at all, rather than an
        // empty one: Linux reports `msg_controllen` 0 there.
        assert!(build_recv_cmsgs(&[], None).is_empty());
    }

    /// Both messages in one buffer. The descriptor message is 20 bytes for one
    /// fd, so the credentials start at 24, not 20: a reader stepping by
    /// `CMSG_ALIGN(cmsg_len)` lands on the header, and one stepping by
    /// `cmsg_len` lands in the middle of it.
    #[test]
    fn descriptors_and_credentials_sit_on_the_alignment_a_reader_steps_by() {
        let creds = ucred_of(7);
        let buf = build_recv_cmsgs(&[9], Some(&creds));
        assert_eq!(buf.len(), 24 + CMSG_HDR_LEN + 12);
        // The fd walk finds the descriptor and stops there.
        assert_eq!(parse_scm_rights_fds(&buf), Ok(vec![9]));
        // The second header begins at the aligned offset.
        assert_eq!(
            i32::from_ne_bytes(buf[24 + 12..24 + 16].try_into().unwrap()),
            SCM_CREDENTIALS
        );
        assert_eq!(buf[24 + 16..], creds[..]);
        // The padding the alignment introduced is zero, not whatever the
        // kernel stack held.
        assert_eq!(&buf[20..24], &[0, 0, 0, 0]);
    }

    #[test]
    fn a_trailing_partial_descriptor_is_not_one() {
        // `cmsg_len` covering 6 bytes of payload is one `int` and a half; the
        // kernel divides and drops the remainder rather than reading past it.
        let ctrl = cmsg_raw(
            (CMSG_HDR_LEN + 6) as u64,
            SOL_SOCKET_LEVEL,
            SCM_RIGHTS,
            &[1, 0, 0, 0, 2, 0],
        );
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![1]));
    }

    #[test]
    fn a_length_past_the_end_of_the_buffer_stops_the_walk() {
        let ctrl = cmsg_raw(4096, SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3, 4]));
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![]));
    }

    #[test]
    fn a_length_that_wraps_the_address_space_stops_the_walk() {
        // `off + cmsg_len` overflowing would land back under `ctrl.len()` and
        // then slice with start > end, which is a kernel panic.
        for len in [u64::MAX, u64::MAX - 7, (usize::MAX as u64) - 16] {
            let ctrl = cmsg_raw(len, SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3, 4]));
            assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![]), "cmsg_len {}", len);
        }
    }

    #[test]
    fn a_length_that_wraps_past_a_message_already_walked_stops_the_walk() {
        // The wrap that actually bites. At offset 0 a huge `cmsg_len` lands
        // past the end of the buffer and is refused on size alone; it takes a
        // first, well-formed message to move the walk forward before
        // `off + cmsg_len` can come back around to a SMALL number that passes
        // the bounds check. The slice is then `ctrl[off+16..that]`, with the
        // start past the end — a kernel panic inside `sendmsg`, from a control
        // buffer any process can hand over.
        let mut ctrl = cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3]));
        assert_eq!(ctrl.len(), 24);
        ctrl.extend(cmsg_raw(u64::MAX - 20, SOL_SOCKET_LEVEL, SCM_RIGHTS, &[]));
        assert_eq!(ctrl.len(), 40);
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![3]));
    }

    #[test]
    fn a_length_shorter_than_its_own_header_stops_the_walk() {
        for len in [0u64, 1, 8, 15] {
            let ctrl = cmsg_raw(len, SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3, 4]));
            assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![]), "cmsg_len {}", len);
        }
    }

    #[test]
    fn an_empty_message_is_stepped_over_rather_than_looped_on() {
        // `cmsg_len` 16 is a well-formed SCM_RIGHTS carrying nothing. The walk
        // has to step past it and find the next one.
        //
        // The `s > 0` guard on the step is a second lock on the same door: a
        // step can only be 0 if `cmsg_len` is, and a `cmsg_len` under 16 has
        // already ended the walk. It is kept because losing the length check
        // would otherwise turn into an unkillable loop inside a syscall rather
        // than a wrong answer, and no test can tell the two locks apart.
        let mut ctrl = cmsg_raw(16, SOL_SOCKET_LEVEL, SCM_RIGHTS, &[]);
        ctrl.extend(cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[8])));
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![8]));
    }

    #[test]
    fn a_buffer_too_short_for_a_header_carries_nothing() {
        for n in 0..CMSG_HDR_LEN {
            let ctrl = vec![0xffu8; n];
            assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![]), "{} bytes", n);
        }
    }

    #[test]
    fn the_descriptor_cap_is_the_number_linux_uses() {
        // Every other test here builds its fd array *from* the constant, so
        // they all move with it and none of them would notice it changing.
        // This is an ABI number (`SCM_MAX_FD`, include/net/scm.h), not a knob.
        assert_eq!(SCM_MAX_FD, 253);
    }

    #[test]
    fn more_descriptors_than_linux_allows_is_einval() {
        // Without the cap, one sendmsg with the 64 KiB control buffer the
        // caller is allowed asks the receiver to install some 16000 fds.
        let ok = cmsg(
            SOL_SOCKET_LEVEL,
            SCM_RIGHTS,
            &fd_bytes(&(0..SCM_MAX_FD as i32).collect::<Vec<_>>()),
        );
        assert_eq!(parse_scm_rights_fds(&ok).map(|v| v.len()), Ok(SCM_MAX_FD));

        let too_many = cmsg(
            SOL_SOCKET_LEVEL,
            SCM_RIGHTS,
            &fd_bytes(&(0..SCM_MAX_FD as i32 + 1).collect::<Vec<_>>()),
        );
        assert_eq!(parse_scm_rights_fds(&too_many), Err(LxError::EINVAL));
    }

    #[test]
    fn the_cap_counts_across_messages_and_not_within_one() {
        // Two messages of 200 fds each is 400, which Linux refuses just the
        // same as one message of 400.
        let half = || fd_bytes(&(0..200i32).collect::<Vec<_>>());
        let mut ctrl = cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &half());
        ctrl.extend(cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &half()));
        assert_eq!(parse_scm_rights_fds(&ctrl), Err(LxError::EINVAL));
    }

    #[test]
    fn a_negative_descriptor_number_comes_back_as_it_was_written() {
        // The walk reports what is on the wire; `collect_scm_rights_fds` is
        // the half that turns a number into a file and answers EBADF.
        let ctrl = cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3, -1, 4]));
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![3, -1, 4]));
    }

    #[test]
    fn what_recvmsg_writes_is_what_sendmsg_reads() {
        // The one that keeps the two halves of the header honest.
        for fds in [
            vec![],
            vec![0],
            vec![3, 4, 5],
            vec![i32::MAX, 1, 2, 3, 4, 5, 6, 7],
        ] {
            let built = build_scm_rights_cmsg(&fds);
            assert_eq!(parse_scm_rights_fds(&built), Ok(fds.clone()), "{:?}", fds);
        }
    }

    #[test]
    fn the_built_message_is_the_length_it_declares() {
        // A caller walks the buffer with `cmsg_len`, so a header that lies
        // about its own size sends it into the next message or off the end.
        let built = build_scm_rights_cmsg(&[3, 4, 5]);
        let declared = u64::from_ne_bytes(built[..8].try_into().unwrap()) as usize;
        assert_eq!(declared, built.len());
        assert_eq!(built.len(), CMSG_HDR_LEN + 3 * 4);
        assert_eq!(
            i32::from_ne_bytes(built[8..12].try_into().unwrap()),
            SOL_SOCKET_LEVEL
        );
        assert_eq!(
            i32::from_ne_bytes(built[12..16].try_into().unwrap()),
            SCM_RIGHTS
        );
    }

    #[test]
    fn getsockopt_reports_what_it_wrote_and_not_what_it_had() {
        // A `struct ucred` is 12 bytes; a caller asking SO_PEERCRED into an
        // `int` gets 4 and must be told 4, or it reads 8 bytes of its own
        // stack as if the kernel had filled them.
        assert_eq!(sockopt_out_len(4, 12), 4);
        assert_eq!(sockopt_out_len(12, 12), 12);
        assert_eq!(sockopt_out_len(64, 12), 12);
        assert_eq!(sockopt_out_len(0, 12), 0);
    }
}

#[cfg(test)]
mod send_mode_tests {
    //! `sock_sendmsg` flag handling for `sendto`/`sendmsg`/`sendmmsg`, which
    //! ignored `flags` altogether: `MSG_DONTWAIT` and `MSG_NOSIGNAL` are the
    //! two a stream client leans on (libwayland sends with both).

    use super::{send_mode, MSG_DONTWAIT, MSG_NOSIGNAL, MSG_PEEK};

    #[test]
    fn a_blocking_unix_socket_waits_and_gets_sigpipe_by_default() {
        let mode = send_mode(0, false, true);
        assert!(mode.wait);
        assert!(mode.sigpipe);
        // Unrelated flags change nothing.
        assert_eq!(send_mode(MSG_PEEK, false, true), mode);
    }

    #[test]
    fn dontwait_or_o_nonblock_or_another_family_means_no_waiting() {
        assert!(!send_mode(MSG_DONTWAIT, false, true).wait);
        assert!(!send_mode(0, true, true).wait);
        // TCP's `write` waits for window itself.
        assert!(!send_mode(0, false, false).wait);
        // None of that touches the signal.
        assert!(send_mode(MSG_DONTWAIT, true, false).sigpipe);
    }

    #[test]
    fn nosignal_is_the_one_way_out_of_sigpipe() {
        let mode = send_mode(MSG_NOSIGNAL, false, true);
        assert!(!mode.sigpipe);
        assert!(mode.wait, "MSG_NOSIGNAL says nothing about waiting");
        assert!(!send_mode(MSG_NOSIGNAL | MSG_DONTWAIT, false, true).wait);
    }
}

#[cfg(test)]
mod peercred_tests {
    //! `SO_PEERCRED` answered `uid 0, gid 0` for every peer, whoever it was.
    //! dbus-daemon, polkit and logind decide who a client IS from this
    //! answer, so every unprivileged client was root to them.

    use super::*;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::ROOT_JOB;

    fn a_process(pid: KoID) -> Arc<Process> {
        Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            pid,
            "peer",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap()
    }

    fn words(bytes: [u8; 12]) -> [u32; 3] {
        let w = |i: usize| u32::from_ne_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        [w(0), w(1), w(2)]
    }

    #[test]
    fn the_ucred_carries_the_peer_s_effective_uid_and_gid() {
        // A root daemon running with EFFECTIVE uid 1000 / gid 100 and its
        // real and saved ids still 0: the effective pair is what
        // `cred_to_ucred` reports, not the real one.
        let peer = a_process(43_101);
        peer.linux().set_resgid(0, 100, 0).unwrap();
        peer.linux().set_resuid(0, 1000, 0).unwrap();
        assert_eq!(words(ucred_of(43_101)), [43_101, 1000, 100]);
    }

    #[test]
    fn a_peer_that_no_longer_exists_has_no_ids() {
        // Linux's `overflowuid`: the answer for credentials it does not hold.
        assert_eq!(words(ucred_of(43_102)), [43_102, u32::MAX, u32::MAX]);
    }
}

#[cfg(test)]
mod bounded_queue_tests {
    //! Which sockets a blocking `send` has to WAIT on: the ones whose `write`
    //! queues into a bounded buffer and answers `EAGAIN` when it is full.
    //! `send_mode`'s tests pass that answer in as a `bool`, so the question of
    //! which sockets it is true for had nobody asking it: with the two arms
    //! ANDed instead of ORed nothing is bounded at all, and a blocking
    //! `sendto` on a full unix socket goes back to answering `EAGAIN` --
    //! which is the bug this pair was written for.

    use super::*;

    #[test]
    fn a_unix_socket_queues_into_a_bounded_buffer() {
        let unix: Arc<dyn FileLike> = UnixSocketState::new();
        assert!(
            queue_is_bounded(&unix),
            "a unix socket writes into its peer's bounded buffer"
        );
    }
}

#[cfg(test)]
mod sockopt_out_tests {
    //! `optlen` is a value-result argument: `sock_getsockopt()` clamps
    //! (`if (len > lv) len = lv;`) before its `put_user(len, optlen)`. Telling
    //! a caller the option's full size instead says the kernel filled bytes it
    //! never touched, and the caller reads its own uninitialised stack as part
    //! of the answer. `SO_PEERCRED` (a 12-byte `struct ucred`) into an `int`
    //! is the case that turns up.

    use super::*;

    /// `SO_TYPE` is optname 3; it used to fall through as `ENOPROTOOPT`.
    /// `SO_BROADCAST` is 6; without the enum arm, getsockopt was ENOPROTOOPT
    /// after setsockopt accepted it.
    #[test]
    fn so_type_is_a_known_sol_socket_optname() {
        assert_eq!(SolOptname::try_from(3usize), Ok(SolOptname::TYPE));
        assert_eq!(SolOptname::TYPE as usize, 3);
        assert_eq!(SolOptname::try_from(6usize), Ok(SolOptname::BROADCAST));
        assert_eq!(SolOptname::try_from(9usize), Ok(SolOptname::KEEPALIVE));
        assert_eq!(SolOptname::try_from(15usize), Ok(SolOptname::REUSEPORT));
        assert_eq!(SolOptname::try_from(20usize), Ok(SolOptname::RCVTIMEO));
        assert_eq!(SolOptname::try_from(21usize), Ok(SolOptname::SNDTIMEO));
        assert_eq!(SolOptname::try_from(30usize), Ok(SolOptname::ACCEPTCONN));
        assert_eq!(TcpOptname::try_from(1usize), Ok(TcpOptname::NODELAY));
        assert_eq!(TcpOptname::try_from(4usize), Ok(TcpOptname::KEEPIDLE));
        assert_eq!(TcpOptname::try_from(5usize), Ok(TcpOptname::KEEPINTVL));
        assert_eq!(TcpOptname::try_from(6usize), Ok(TcpOptname::KEEPCNT));
        assert_eq!(TcpOptname::try_from(13usize), Ok(TcpOptname::CONGESTION));
        assert_eq!(IpOptname::try_from(1usize), Ok(IpOptname::TOS));
        assert_eq!(IpOptname::try_from(2usize), Ok(IpOptname::TTL));
        assert_eq!(IpOptname::try_from(3usize), Ok(IpOptname::HDRINCL));
        assert_eq!(IpOptname::try_from(32usize), Ok(IpOptname::MulticastIf));
        assert_eq!(IpOptname::try_from(33usize), Ok(IpOptname::MulticastTtl));
        assert_eq!(IpOptname::try_from(35usize), Ok(IpOptname::MulticastLoop));
    }

    /// A level this kernel does not wire up is `ENOPROTOOPT`, not a fake 0.
    #[test]
    fn unknown_getsockopt_levels_are_not_sol_socket_or_ip() {
        // The three `Level` knows; anything else used to succeed with 0.
        assert!(Level::try_from(1usize).is_ok()); // SOL_SOCKET
        assert!(Level::try_from(0usize).is_ok()); // IPPROTO_IP
        assert!(Level::try_from(6usize).is_ok()); // IPPROTO_TCP
        assert!(Level::try_from(263usize).is_err()); // SOL_PACKET
        assert!(Level::try_from(41usize).is_err()); // SOL_IPV6
        assert!(Level::try_from(99usize).is_err());
    }

    // `libos` addresses are ordinary host addresses, so a local buffer is a
    // valid stand-in for the caller's and the copy below runs for real.
    fn out(buf: &mut [u8]) -> UserOutPtr<u32> {
        UserOutPtr::from(buf.as_mut_ptr() as usize)
    }
    fn in_out(slot: &mut u32) -> UserInOutPtr<u32> {
        UserInOutPtr::from(slot as *mut u32 as usize)
    }

    #[test]
    fn a_buffer_smaller_than_the_option_is_told_what_was_written() {
        let value = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let mut buf = [0xEEu8; 16];
        let mut len = 4u32;
        let optlen = in_out(&mut len);
        assert_eq!(write_sockopt_out(out(&mut buf), optlen, &value), Ok(0));
        assert_eq!(len, 4, "the caller was told more than the kernel wrote");
        assert_eq!(buf[..4], value[..4]);
        assert_eq!(
            buf[4..],
            [0xEEu8; 12],
            "the kernel wrote past the buffer the caller supplied"
        );
    }

    #[test]
    fn a_buffer_larger_than_the_option_is_told_the_option_s_size() {
        let value = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let mut buf = [0xEEu8; 32];
        let mut len = 32u32;
        let optlen = in_out(&mut len);
        assert_eq!(write_sockopt_out(out(&mut buf), optlen, &value), Ok(0));
        assert_eq!(len, 12, "a roomy buffer is told the option's real size");
        assert_eq!(buf[..12], value[..]);
        assert_eq!(buf[12..], [0xEEu8; 20], "wrote past the option's value");
    }

    #[test]
    fn a_buffer_of_no_length_is_not_written_at_all() {
        // `getsockopt(fd, ..., &val, &zero)` is how a caller asks only for the
        // size, and it must not take a single byte of `val`.
        let value = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let mut buf = [0xEEu8; 16];
        let mut len = 0u32;
        let optlen = in_out(&mut len);
        assert_eq!(write_sockopt_out(out(&mut buf), optlen, &value), Ok(0));
        assert_eq!(len, 0);
        assert_eq!(buf, [0xEEu8; 16], "a zero-length buffer was written");
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod read_sockaddr_tests {
    //! `move_addr_to_kernel`: copy exactly the `addrlen` the caller declared,
    //! at one-byte alignment, and leave the rest zero. Reading the whole
    //! `SockAddr` union instead had two bugs -- it needed 4-byte alignment,
    //! which a `sockaddr_un` does not have (this is what answered "unable to
    //! connect to X server: Bad address"), and it over-read past a short user
    //! buffer.

    use super::*;

    #[test]
    fn a_null_address_is_efault_even_with_nothing_to_read() {
        // `addrlen == 0` is the length that tells the two apart: with no bytes
        // to copy, the NULL check is the only thing left to refuse it, and
        // without it `bind(fd, NULL, 0)` comes back with a zeroed address
        // instead of an error.
        for len in [0usize, 1, 2, size_of::<SockAddr>(), usize::MAX] {
            assert_eq!(
                read_sockaddr(0, len).err(),
                Some(LxError::EFAULT),
                "addrlen {}",
                len
            );
        }
    }

    #[test]
    fn only_the_bytes_the_caller_declared_are_copied() {
        // A caller that declares two bytes must not have the 106 bytes of
        // path that happen to follow them dragged in as its socket's name.
        let src = [0xAAu8; size_of::<SockAddr>()];
        let sa = read_sockaddr(src.as_ptr() as usize, 2).expect("two bytes are readable");
        let un = unsafe { sa.addr_un };
        assert_eq!(un.sun_family, 0xAAAA);
        assert!(
            un.sun_path.iter().all(|&b| b == 0),
            "the path came from memory the caller never declared"
        );
    }

    #[test]
    fn a_single_declared_byte_is_still_copied() {
        let src = [0x7Fu8; size_of::<SockAddr>()];
        let sa = read_sockaddr(src.as_ptr() as usize, 1).expect("one byte is readable");
        let family = unsafe { sa.family }.to_ne_bytes();
        assert_eq!(family[0], 0x7F, "the one byte declared never arrived");
        assert_eq!(family[1], 0, "a byte the caller did not declare arrived");
    }
}

#[cfg(test)]
mod socket_type_flag_tests {
    //! `socket` and `socketpair` pack SOCK_NONBLOCK/SOCK_CLOEXEC into the
    //! type word. The high bits used to go through `from_bits_truncate`, so a
    //! stray bit was success (or worse, an unrelated `OpenFlags` name).

    use super::*;
    use crate::file::{anon_fd_flags, ANON_CLOEXEC, ANON_NONBLOCK};

    const SOCK_FLAGS: usize = ANON_CLOEXEC | ANON_NONBLOCK;

    #[test]
    fn the_two_known_bits_survive_and_nothing_else() {
        let f = anon_fd_flags(ANON_CLOEXEC | ANON_NONBLOCK, SOCK_FLAGS).unwrap();
        assert!(f.close_on_exec() && f.non_block());
        let none = anon_fd_flags(0, SOCK_FLAGS).unwrap();
        assert!(!none.close_on_exec() && !none.non_block());
    }

    #[test]
    fn a_stray_high_bit_is_einval_not_a_working_socket() {
        // Bit that lands on `OpenFlags::APPEND` if truncated — the silent
        // mis-apply case, not just the silent-ignore case.
        const APPEND: usize = 0o2000;
        assert_eq!(
            anon_fd_flags(APPEND, SOCK_FLAGS),
            Err(LxError::EINVAL),
            "APPEND must not become a socket open flag"
        );
        assert_eq!(
            anon_fd_flags(1usize << 30, SOCK_FLAGS),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            anon_fd_flags(APPEND | ANON_CLOEXEC, SOCK_FLAGS),
            Err(LxError::EINVAL),
            "a valid bit must not hide a stray one"
        );
    }

    #[test]
    fn the_type_nibble_is_stripped_before_the_flag_check() {
        // Callers pass `SOCK_STREAM | SOCK_NONBLOCK`; only the high bits
        // reach `anon_fd_flags`.
        let packed = (SocketType::SOCK_STREAM as usize) | ANON_NONBLOCK;
        let flag_bits = packed & !SOCKET_TYPE_MASK;
        let f = anon_fd_flags(flag_bits, SOCK_FLAGS).unwrap();
        assert!(f.non_block());
        assert!(!f.close_on_exec());
    }

    /// `socket` / `socketpair` on AF_UNIX used to accept every `SocketType`
    /// (or, for socketpair, swallow unknowns as SOCK_STREAM). The three
    /// Linux knows succeed; garbage is EINVAL; named-but-unsupported types
    /// are EOPNOTSUPP.
    #[test]
    fn af_unix_refuses_a_type_that_is_not_a_unix_type() {
        assert_eq!(
            unix_socket_type(SocketType::SOCK_STREAM),
            Ok(SocketType::SOCK_STREAM)
        );
        assert_eq!(
            unix_socket_type(SocketType::SOCK_DGRAM),
            Ok(SocketType::SOCK_DGRAM)
        );
        assert_eq!(
            unix_socket_type(SocketType::SOCK_SEQPACKET),
            Ok(SocketType::SOCK_SEQPACKET)
        );
        assert_eq!(
            unix_socket_type(SocketType::SOCK_RAW),
            Err(LxError::EOPNOTSUPP)
        );
        assert_eq!(
            unix_socket_type(SocketType::SOCK_RDM),
            Err(LxError::EOPNOTSUPP)
        );
        assert_eq!(
            unix_socket_type(SocketType::SOCK_PACKET),
            Err(LxError::EOPNOTSUPP)
        );
        assert_eq!(unix_socketpair_type(99), Err(LxError::EINVAL));
        assert_eq!(unix_socketpair_type(0), Err(LxError::EINVAL));
        assert_eq!(
            unix_socketpair_type(SocketType::SOCK_STREAM as usize),
            Ok(SocketType::SOCK_STREAM)
        );
    }

    /// `unix_create` only accepts protocol 0 or `PF_UNIX`. Any other value
    /// used to succeed on both `socket` and `socketpair`.
    #[test]
    fn af_unix_refuses_a_protocol_that_is_not_unix() {
        assert_eq!(unix_protocol(0), Ok(()));
        assert_eq!(unix_protocol(Domain::AF_UNIX as usize), Ok(()));
        assert_eq!(unix_protocol(99), Err(LxError::EPROTONOSUPPORT));
        assert_eq!(unix_protocol(6), Err(LxError::EPROTONOSUPPORT)); // IPPROTO_TCP
    }

    /// `socket(AF_INET, SOCK_DGRAM, 99)` used to succeed as UDP because
    /// `Protocol::try_from` failed → `None` matched a "tolerant" arm.
    /// Linux says `EPROTONOSUPPORT`; only 0/`IPPROTO_UDP`/`IPPROTO_ICMP`
    /// are wired for datagram.
    #[test]
    fn an_unknown_inet_dgram_protocol_is_not_silently_udp() {
        assert!(Protocol::try_from(0usize).is_ok()); // IPPROTO_IP → UDP arm
        assert!(Protocol::try_from(17usize).is_ok()); // IPPROTO_UDP
        assert!(Protocol::try_from(1usize).is_ok()); // IPPROTO_ICMP (ping)
        assert!(
            Protocol::try_from(99usize).is_err(),
            "unknown protocol must not become None→UDP; catch-all is EPROTONOSUPPORT"
        );
    }

    /// `getpeername` with no peer must be `ENOTCONN`, not `EINVAL`.
    #[test]
    fn getpeername_without_a_peer_is_enotconn() {
        assert_eq!(peer_endpoint(None).err(), Some(LxError::ENOTCONN));
    }
}
